use std::io;
use std::os::unix::io::RawFd;

use alsa::mixer::{SelemChannelId, SelemId};
use alsa::{self, Mixer, PollDescriptors};
use failure::{format_err, Error, ResultExt};
use futures::{Async, Poll, Stream};
use mio::event::Evented;
use mio::unix::EventedFd;
use mio::{self, PollOpt, Ready, Token};
use tokio_core::reactor::{Handle, PollEvented};

use super::{Widget, WidgetStream};
use crate::text::{Attributes, Text};
use crate::{Cnx, Result};

/// Shows the current volume of the default ALSA output.
///
/// This widget shows the current volume of the default ALSA output, or '`M`' if
/// the output is muted.
///
/// The widget uses `alsa-lib` to receive events when the volume changes,
/// avoiding expensive polling. If you do not have `alsa-lib` installed, you
/// can disable the `volume-widget` feature on the `cnx` crate to avoid
/// compiling this widget.
pub struct Volume {
    handle: Handle,
    attr: Attributes,
}

impl Volume {
    /// Creates a new Volume widget.
    ///
    /// Creates a new `Volume` widget, whose text will be displayed
    /// with the given [`Attributes`].
    ///
    /// The [`Cnx`] instance is borrowed during construction in order to get
    /// access to handles of its event loop. However, it is not borrowed for the
    /// lifetime of the widget. See the [`cnx_add_widget!()`] for more discussion
    /// about the lifetime of the borrow.
    ///
    /// [`Attributes`]: ../text/struct.Attributes.html
    /// [`Cnx`]: ../struct.Cnx.html
    /// [`cnx_add_widget!()`]: ../macro.cnx_add_widget.html
    ///
    /// # Examples
    ///
    /// ```
    /// # #[macro_use]
    /// # extern crate cnx;
    /// #
    /// # use cnx::*;
    /// # use cnx::text::*;
    /// # use cnx::widgets::*;
    /// #
    /// # fn run() -> ::cnx::Result<()> {
    /// let attr = Attributes {
    ///     font: Font::new("SourceCodePro 21"),
    ///     fg_color: Color::white(),
    ///     bg_color: None,
    ///     padding: Padding::new(8.0, 8.0, 0.0, 0.0),
    /// };
    ///
    /// let mut cnx = Cnx::new(Position::Top)?;
    /// cnx_add_widget!(cnx, Volume::new(&cnx, attr.clone()));
    /// # Ok(())
    /// # }
    /// # fn main() { run().unwrap(); }
    /// ```
    pub fn new(cnx: &Cnx, attr: Attributes) -> Volume {
        Volume {
            handle: cnx.handle(),
            attr,
        }
    }
}

impl Widget for Volume {
    fn stream(self: Box<Self>) -> Result<WidgetStream> {
        let mixer_name = "default";
        // We don't attempt to use the same mixer to listen for events and to
        // recompute the mixer state (in the callback below) as the Mixer seems
        // to cache the state from when it was created. It's relatively cheap
        // create a new mixer each time we get an event though.
        let mixer = Mixer::new(mixer_name, true)
            .with_context(|_| format!("Failed to open ALSA mixer: {}", mixer_name))?;
        let stream = AlsaEventStream::new(&self.handle, mixer)?
            .and_then(move |()| {
                // FrontLeft has special meaning in ALSA and is the channel
                // that's used when the mixer is mono.
                let channel = SelemChannelId::FrontLeft;

                let mixer = Mixer::new(mixer_name, true)?;
                let master = mixer
                    .find_selem(&SelemId::new("Master", 0))
                    .ok_or_else(|| format_err!("Couldn't open Master channel"))?;

                let mute = master.get_playback_switch(channel)? == 0;

                let text = if !mute {
                    let volume = master.get_playback_volume(channel)?;
                    let (min, max) = master.get_playback_volume_range();
                    let percentage = (volume as f64 / (max as f64 - min as f64)) * 100.0;
                    format!("{:.0}%", percentage)
                } else {
                    "M".to_owned()
                };

                Ok(vec![Text {
                    attr: self.attr.clone(),
                    text: text,
                    stretch: false,
                }])
            })
            .then(|r| r.context("Error getting ALSA volume information"))
            .map_err(|e| e.into());

        Ok(Box::new(stream))
    }
}

struct AlsaEvented(Mixer);

impl AlsaEvented {
    fn mixer(&self) -> &Mixer {
        &self.0
    }

    fn fds(&self) -> Vec<RawFd> {
        self.0
            .get()
            .unwrap()
            .iter()
            .map(|pollfd| pollfd.fd)
            .collect()
    }
}

impl Evented for AlsaEvented {
    fn register(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        for fd in &self.fds() {
            EventedFd(fd).register(poll, token, interest, opts)?
        }
        Ok(())
    }

    fn reregister(
        &self,
        poll: &mio::Poll,
        token: Token,
        interest: Ready,
        opts: PollOpt,
    ) -> io::Result<()> {
        for fd in &self.fds() {
            EventedFd(fd).reregister(poll, token, interest, opts)?
        }
        Ok(())
    }

    fn deregister(&self, poll: &mio::Poll) -> io::Result<()> {
        // XXX If the set of fds changes (it won't), should we deregister the
        // original set?
        for fd in &self.fds() {
            EventedFd(fd).deregister(poll)?
        }
        Ok(())
    }
}

struct AlsaEventStream {
    poll: PollEvented<AlsaEvented>,
    initial: bool,
}

impl AlsaEventStream {
    fn new(handle: &Handle, mixer: Mixer) -> Result<AlsaEventStream> {
        Ok(AlsaEventStream {
            poll: PollEvented::new(AlsaEvented(mixer), handle)?,
            // The first few calls to poll() need to process any existing events.
            // We don't know what state the fds are in when we give them to tokio
            // and it's edge-triggered.
            initial: true,
        })
    }
}

impl Stream for AlsaEventStream {
    // We don't bother yielding the events and just yield unit each time we get
    // an event. This stream is used only to get woken up when the ALSA state
    // changes - the caller is expected to requery all necessary state when
    // it receives a new item from the stream.
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // Always assume we're ready initially, so that we can clear the
        // state of the fds.
        if !self.initial {
            if let Async::NotReady = self.poll.poll_read() {
                return Ok(Async::NotReady);
            }
        }
        self.initial = false;

        // Do a poll with a timeout of 0 to figure out exactly which fds were
        // woken up, followed by a call to revents() which clears the pending
        // events. We don't actually care what the events are - we're just
        // using it as a wake-up so we can check the volume again.
        let mixer = self.poll.get_ref().mixer();
        let ready = alsa::poll::poll_all(&[mixer], 0)?;
        let poll_descriptors = ready.into_iter().map(|(p, _)| p);
        for poll_descriptor in poll_descriptors {
            mixer.revents(poll_descriptor.get()?.as_slice())?;
        }
        // All events have been consumed - tell Tokio we're interested in waiting
        // for more again.
        self.poll.need_read();

        Ok(Async::Ready(Some(())))
    }
}
