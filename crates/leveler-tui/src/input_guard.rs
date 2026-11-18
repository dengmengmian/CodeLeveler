//! Separates solicited terminal device replies from user input.
//!
//! Windows Terminal answers a DA1 query (`ESC [ c`) with a sequence such as
//! `ESC [ ? 61 ; 4 ; ... c`. Crossterm delivers that reply as ordinary key
//! events, so when the reply is late — a busy host, a resumed terminal — it
//! reaches the composer and is submitted as if the user had typed it. Draining
//! a fixed window after the query cannot close that: the reply may arrive at
//! any later time.
//!
//! This filter recognizes the complete private-CSI reply grammar and drops it
//! at the input boundary, before coalescing or the reducer. Only a syntactically
//! complete DA1/DA2 reply is dropped:
//!
//! ```text
//! [ESC] '[' ('?' | '>') parameters 'c'
//! parameters := [0-9;]*
//! ```
//!
//! A partial sequence is buffered under a short deadline; anything that stops
//! matching is replayed to the user in its original order. Pasted text arrives
//! as `Event::Paste` and never enters this grammar.

use std::collections::VecDeque;
use std::time::Duration;

use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEventKind};
use futures::StreamExt;

use crate::action::Action;

/// How long a lone `ESC` or `[` waits to see whether a device reply follows.
/// Device replies arrive as one burst; a human's following keystroke does not
/// need to wait for the terminal round trip.
const PREFIX_TIMEOUT: Duration = Duration::from_millis(30);
/// How long a syntactically valid but incomplete reply may stay buffered.
/// This covers a fragmented reply without holding a user's literal `[?` text
/// indefinitely.
const REPLY_TIMEOUT: Duration = Duration::from_millis(2_000);
/// A private CSI reply is short; a longer parameter run is user input.
const MAX_PARAMETERS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// `ESC` seen, waiting for `[`.
    Escape,
    /// `[` seen, waiting for `?`/`>`.
    Bracket,
    /// `[?` or `[>` seen; parameters accumulate until `c`.
    Reply,
}

/// The pure state machine. It never inspects the rest of the application.
#[derive(Debug)]
pub(crate) struct DeviceReplyFilter {
    state: State,
    buffer: Vec<Action>,
    parameters: String,
    #[cfg_attr(not(windows), allow(dead_code))]
    last_reply: Option<String>,
}

impl Default for DeviceReplyFilter {
    fn default() -> Self {
        Self {
            state: State::Idle,
            buffer: Vec::new(),
            parameters: String::new(),
            last_reply: None,
        }
    }
}

impl DeviceReplyFilter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed one action. Returns the actions that are user input, in order.
    ///
    /// A complete device reply produces an empty list; a non-matching prefix
    /// is returned exactly as it arrived.
    pub(crate) fn push(&mut self, action: Action) -> Vec<Action> {
        let mut output = Vec::new();
        match action {
            // Paste is bracketed by the terminal and cannot be a device reply.
            Action::Paste(_) | Action::Mouse(_) | Action::Resize(_, _) | Action::TextInput(_) => {
                output.extend(self.flush());
                output.push(action);
                return output;
            }
            Action::Key(key) => {
                // Windows delivers a release record after every press. A
                // release carries no text and every binding acts on the
                // press; letting one interrupt a CSI sequence would split the
                // reply into user input.
                if key.kind == KeyEventKind::Release {
                    return output;
                }
                let mut pending = Some(key);
                while let Some(key) = pending.take() {
                    match self.state {
                        State::Idle => {
                            match key.code {
                                KeyCode::Esc => {
                                    self.state = State::Escape;
                                    self.buffer.push(Action::Key(key));
                                }
                                KeyCode::Char('[') => {
                                    // Crossterm can strip the leading ESC while
                                    // parsing a CSI sequence.
                                    self.state = State::Bracket;
                                    self.buffer.push(Action::Key(key));
                                }
                                _ => output.push(Action::Key(key)),
                            }
                        }
                        State::Escape => match key.code {
                            KeyCode::Char('[') => {
                                self.state = State::Bracket;
                                self.buffer.push(Action::Key(key));
                            }
                            _ => {
                                // Not a device reply: the ESC was the user's.
                                output.extend(self.flush());
                                pending = Some(key);
                            }
                        },
                        State::Bracket => match key.code {
                            KeyCode::Char('?') | KeyCode::Char('>') => {
                                self.parameters.clear();
                                self.parameters.push(match key.code {
                                    KeyCode::Char('?') => '?',
                                    _ => '>',
                                });
                                self.state = State::Reply;
                                self.buffer.push(Action::Key(key));
                            }
                            _ => {
                                output.extend(self.flush());
                                pending = Some(key);
                            }
                        },
                        State::Reply => match key.code {
                            KeyCode::Char(character)
                                if character.is_ascii_digit() || character == ';' =>
                            {
                                if self.parameters.len() >= MAX_PARAMETERS {
                                    output.extend(self.flush());
                                    pending = Some(key);
                                } else {
                                    self.parameters.push(character);
                                    self.buffer.push(Action::Key(key));
                                }
                            }
                            KeyCode::Char('c') => {
                                // A complete DA1/DA2 reply. It is a terminal
                                // answer, never text.
                                self.buffer.push(Action::Key(key));
                                self.last_reply = Some(format!("[{}c", self.parameters));
                                self.discard();
                            }
                            _ => {
                                output.extend(self.flush());
                                pending = Some(key);
                            }
                        },
                    }
                }
            }
            other => output.push(other),
        }
        output
    }

    /// Release any buffered prefix as user input. Called when the deadline
    /// expires or another event kind interrupts the burst.
    pub(crate) fn flush(&mut self) -> Vec<Action> {
        self.state = State::Idle;
        self.parameters.clear();
        std::mem::take(&mut self.buffer)
    }

    fn discard(&mut self) {
        self.state = State::Idle;
        self.parameters.clear();
        self.buffer.clear();
    }

    /// The most recent complete device reply, e.g. `[?61;4c`. Used by the
    /// startup capability probe; unrelated to user input.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn take_completed_reply(&mut self) -> Option<String> {
        self.last_reply.take()
    }

    /// How long the caller may wait before releasing a buffered prefix.
    pub(crate) fn deadline(&self) -> Option<Duration> {
        match self.state {
            State::Escape | State::Bracket => Some(PREFIX_TIMEOUT),
            State::Reply => Some(REPLY_TIMEOUT),
            State::Idle => None,
        }
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn is_buffering(&self) -> bool {
        self.state != State::Idle
    }
}

/// Input the startup capability probe already read, in the order it was read.
///
/// A reply prefix the probe consumed is replayed into the runtime filter so
/// its continuation is still recognized; a key the user pressed during the
/// probe is delivered normally.
#[derive(Debug, Default)]
pub(crate) struct StartupInput {
    entries: Vec<StartupEntry>,
}

/// One startup input, tagged with whether the capability probe classified it
/// as a reply prefix. Constructed by the Windows probe and its tests.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug)]
enum StartupEntry {
    User(Action),
    Reply(Action),
}

impl StartupInput {
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn push_user(&mut self, action: Action) {
        self.entries.push(StartupEntry::User(action));
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn push_reply(&mut self, action: Action) {
        self.entries.push(StartupEntry::Reply(action));
    }
}

/// The TUI's terminal event source, with device replies removed.
pub(crate) struct TerminalInput {
    events: EventStream,
    filter: DeviceReplyFilter,
    pending: VecDeque<Action>,
}

impl TerminalInput {
    pub(crate) fn with_startup(startup: StartupInput) -> Self {
        let mut filter = DeviceReplyFilter::new();
        let mut pending = VecDeque::new();
        for entry in startup.entries {
            match entry {
                StartupEntry::User(action) => pending.push_back(action),
                StartupEntry::Reply(action) => {
                    // Rebuild whatever partial state the probe had observed;
                    // anything that stops matching is real user input.
                    pending.extend(filter.push(action));
                }
            }
        }
        Self {
            events: EventStream::new(),
            filter,
            pending,
        }
    }

    /// The next user input action, or the next terminal bookkeeping action.
    ///
    /// Reads until the filter yields something. A buffered reply prefix is
    /// released when its deadline expires, so a lone ESC key still acts
    /// promptly.
    pub(crate) async fn next(&mut self) -> Option<std::io::Result<Action>> {
        loop {
            if let Some(action) = self.pending.pop_front() {
                return Some(Ok(action));
            }
            let deadline = self.filter.deadline();
            let event = match deadline {
                Some(deadline) => {
                    tokio::select! {
                        event = self.events.next() => event,
                        _ = tokio::time::sleep(deadline) => {
                            self.pending.extend(self.filter.flush());
                            continue;
                        }
                    }
                }
                None => self.events.next().await,
            };
            let event = match event {
                Some(Ok(event)) => event,
                Some(Err(error)) => return Some(Err(error)),
                None => return None,
            };
            if let Some(action) = map_event(event) {
                self.pending.extend(self.filter.push(action));
            }
        }
    }
}

fn map_event(event: CtEvent) -> Option<Action> {
    match event {
        CtEvent::Key(key) => Some(Action::Key(key)),
        CtEvent::Mouse(mouse) => Some(Action::Mouse(mouse)),
        CtEvent::Paste(text) => Some(Action::Paste(text)),
        CtEvent::Resize(cols, rows) => Some(Action::Resize(cols, rows)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> Action {
        Action::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn chars(text: &str) -> Vec<Action> {
        text.chars().map(|c| key(KeyCode::Char(c))).collect()
    }

    fn reply(with_escape: bool) -> Vec<Action> {
        let mut actions = Vec::new();
        if with_escape {
            actions.push(key(KeyCode::Esc));
        }
        actions.extend(chars("[?61;4;6;7;14;21;22;23;24;28;32;42;52c"));
        actions
    }

    fn feed(filter: &mut DeviceReplyFilter, actions: Vec<Action>) -> Vec<Action> {
        let mut output = Vec::new();
        for action in actions {
            output.extend(filter.push(action));
        }
        output
    }

    /// Windows sends a release record after every press. Interleave those the
    /// way the console does; they must not break the reply grammar.
    fn with_releases(actions: Vec<Action>) -> Vec<Action> {
        let mut output = Vec::new();
        for action in actions {
            if let Action::Key(key) = &action {
                let release =
                    KeyEvent::new_with_kind(key.code, key.modifiers, KeyEventKind::Release);
                output.push(action);
                output.push(Action::Key(release));
            } else {
                output.push(action);
            }
        }
        output
    }

    /// `Action` deliberately has no `PartialEq` (its runtime variants carry
    /// state that is not a value comparison). Debug rendering is enough to
    /// prove the key stream is preserved exactly.
    fn assert_actions(actual: &[Action], expected: &[Action]) {
        assert_eq!(
            actual
                .iter()
                .map(|action| format!("{action:?}"))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|action| format!("{action:?}"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_complete_da1_reply_is_never_user_input() {
        for with_escape in [false, true] {
            let mut filter = DeviceReplyFilter::new();
            let output = feed(&mut filter, reply(with_escape));
            assert!(output.is_empty(), "{with_escape}: {output:?}");
            assert!(!filter.is_buffering());
        }
    }

    #[test]
    fn a_late_reply_cannot_prepend_itself_to_a_command() {
        let mut filter = DeviceReplyFilter::new();
        let mut actions = reply(false);
        actions.extend(chars("/fork"));
        let output = feed(&mut filter, actions);
        let text: String = output
            .iter()
            .map(|action| match action {
                Action::Key(key) => match key.code {
                    KeyCode::Char(c) => c,
                    other => panic!("unexpected key {other:?}"),
                },
                other => panic!("unexpected action {other:?}"),
            })
            .collect();
        assert_eq!(text, "/fork");
    }

    #[test]
    fn repeated_and_fragmented_replies_are_all_consumed() {
        let mut filter = DeviceReplyFilter::new();
        let mut actions = reply(false);
        actions.extend(reply(true));
        let output = feed(&mut filter, actions);
        assert!(output.is_empty(), "{output:?}");

        // The same reply one event at a time, with the filter observing
        // intermediate states, must still end in a drop.
        let mut filter = DeviceReplyFilter::new();
        let mut output = Vec::new();
        for action in reply(false) {
            output.extend(filter.push(action));
        }
        assert!(output.is_empty(), "{output:?}");
        assert!(!filter.is_buffering());
    }

    #[test]
    fn windows_key_release_records_do_not_split_a_reply() {
        let mut filter = DeviceReplyFilter::new();
        let output = feed(&mut filter, with_releases(reply(true)));
        assert!(output.is_empty(), "{output:?}");
        assert!(!filter.is_buffering());
    }

    #[test]
    fn a_prefix_that_stops_matching_is_replayed_verbatim() {
        let mut filter = DeviceReplyFilter::new();
        let output = feed(
            &mut filter,
            vec![key(KeyCode::Esc), key(KeyCode::Char('['))],
        );
        assert!(output.is_empty(), "a plausible prefix is buffered");
        let output = feed(&mut filter, vec![key(KeyCode::Char('x'))]);
        assert_actions(
            &output,
            &[
                key(KeyCode::Esc),
                key(KeyCode::Char('[')),
                key(KeyCode::Char('x')),
            ],
        );
        assert!(!filter.is_buffering());
    }

    #[test]
    fn an_invalid_parameter_ends_the_reply_and_returns_the_text() {
        let mut filter = DeviceReplyFilter::new();
        let output = feed(&mut filter, chars("[?61x"));
        assert_actions(&output, &chars("[?61x"));
    }

    #[test]
    fn escape_alone_is_released_by_the_deadline() {
        let mut filter = DeviceReplyFilter::new();
        assert!(feed(&mut filter, vec![key(KeyCode::Esc)]).is_empty());
        assert_eq!(filter.deadline(), Some(PREFIX_TIMEOUT));
        assert_actions(&filter.flush(), &[key(KeyCode::Esc)]);
        assert_eq!(filter.deadline(), None);
    }

    #[test]
    fn a_partial_reply_is_held_until_its_longer_deadline() {
        let mut filter = DeviceReplyFilter::new();
        assert!(feed(&mut filter, chars("[?61;")).is_empty());
        assert_eq!(filter.deadline(), Some(REPLY_TIMEOUT));
    }

    #[test]
    fn paste_and_non_key_events_bypass_the_grammar() {
        let mut filter = DeviceReplyFilter::new();
        let pasted = Action::Paste("[?61;4c".into());
        assert_actions(&filter.push(pasted.clone()), &[pasted]);
        assert!(!filter.is_buffering());

        // A paste arriving mid-prefix releases the prefix first, preserving
        // the order the user produced.
        let mut filter = DeviceReplyFilter::new();
        assert!(feed(&mut filter, vec![key(KeyCode::Esc)]).is_empty());
        let output = filter.push(Action::Resize(80, 24));
        assert_actions(&output, &[key(KeyCode::Esc), Action::Resize(80, 24)]);
    }

    #[test]
    fn ordinary_unicode_ime_and_control_keys_are_untouched() {
        let mut filter = DeviceReplyFilter::new();
        let mut actions = chars("你好，世界🙂");
        actions.push(key(KeyCode::Left));
        actions.push(key(KeyCode::Enter));
        actions.push(Action::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        actions.push(Action::Key(KeyEvent::new(
            KeyCode::Char('z'),
            KeyModifiers::CONTROL,
        )));
        let output = feed(&mut filter, actions.clone());
        assert_actions(&output, &actions);
    }

    #[test]
    fn a_reply_arriving_between_two_parts_of_typed_text_leaves_the_text_whole() {
        let mut filter = DeviceReplyFilter::new();
        let mut output = feed(&mut filter, chars("/fo"));
        output.extend(feed(&mut filter, reply(true)));
        output.extend(feed(&mut filter, chars("rk")));
        let text: String = output
            .iter()
            .filter_map(|action| match action {
                Action::Key(key) => match key.code {
                    KeyCode::Char(c) => Some(c),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(text, "/fork");
    }
}
