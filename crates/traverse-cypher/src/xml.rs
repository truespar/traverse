//! Shared pull-parser front end for the XML consumers (`apoc.load.xml`,
//! `apoc.import.xml`, `apoc.import.graphml`).
//!
//! quick-xml reports entity and character references as separate
//! `Event::GeneralRef` events, so `AT&amp;T` arrives as three. The consumers all
//! want the logical text node (one run per stretch of character data, references
//! resolved, as the JVM parsers behind APOC deliver it), so [`XmlEvents`]
//! re-assembles it: text and reference events accumulate and go out as one
//! [`XmlEvent::Text`] at the next piece of markup (tag, comment, processing
//! instruction, end of input). Line ends are normalized to `\n` per the XML spec.
//!
//! CDATA sections are skipped.

use quick_xml::Reader;
use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesEnd, BytesStart, Event};
use std::fmt;

/// Parse, encoding or entity error, already formatted for the user.
#[derive(Debug)]
pub struct XmlError(String);

impl fmt::Display for XmlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for XmlError {}

impl From<quick_xml::Error> for XmlError {
    fn from(e: quick_xml::Error) -> Self {
        XmlError(e.to_string())
    }
}

impl From<quick_xml::encoding::EncodingError> for XmlError {
    fn from(e: quick_xml::encoding::EncodingError) -> Self {
        XmlError(e.to_string())
    }
}

/// Logical event: markup, or one complete run of character data.
pub enum XmlEvent {
    Start(BytesStart<'static>),
    Empty(BytesStart<'static>),
    End(BytesEnd<'static>),
    Text(String),
    Eof,
}

/// Pull parser over an in-memory document, yielding [`XmlEvent`]s.
pub struct XmlEvents<'s> {
    reader: Reader<&'s [u8]>,
    buf: Vec<u8>,
    trim: bool,
    text: String,
    have_text: bool,
    queued: Option<XmlEvent>,
}

impl<'s> XmlEvents<'s> {
    /// Parses `document`. With `trim`, XML whitespace is trimmed from every text run and
    /// whitespace-only runs are dropped (GraphML importer mode); without it character data comes
    /// through verbatim.
    pub fn new(document: &'s str, trim: bool) -> Self {
        let mut reader = Reader::from_str(document);
        // trimming happens on the re-assembled run in `take_text`; per-event trimming would eat
        // whitespace next to a reference (`a &amp; b` -> `a&b`)
        reader.config_mut().trim_text(false);
        Self {
            reader,
            buf: Vec::new(),
            trim,
            text: String::new(),
            have_text: false,
            queued: None,
        }
    }

    /// Next logical event; [`XmlEvent::Eof`] at end of input.
    pub fn next(&mut self) -> Result<XmlEvent, XmlError> {
        if let Some(ev) = self.queued.take() {
            return Ok(ev);
        }
        loop {
            self.buf.clear();
            let markup = match self.reader.read_event_into(&mut self.buf)? {
                Event::Text(t) => {
                    self.text.push_str(&t.xml10_content());
                    self.have_text = true;
                    continue;
                }
                Event::GeneralRef(r) => {
                    match r.resolve_char_ref()? {
                        Some(c) => self.text.push(c),
                        None => {
                            let name: &str = &r;
                            match resolve_predefined_entity(name) {
                                Some(s) => self.text.push_str(s),
                                None => {
                                    return Err(XmlError(format!(
                                        "unrecognized entity reference &{name};"
                                    )));
                                }
                            }
                        }
                    }
                    self.have_text = true;
                    continue;
                }
                Event::Start(e) => Some(XmlEvent::Start(e.into_owned())),
                Event::Empty(e) => Some(XmlEvent::Empty(e.into_owned())),
                Event::End(e) => Some(XmlEvent::End(e.into_owned())),
                Event::Eof => Some(XmlEvent::Eof),
                // skipped, but they still end a text run
                Event::CData(_)
                | Event::Comment(_)
                | Event::PI(_)
                | Event::Decl(_)
                | Event::DocType(_) => None,
            };
            if let Some(text) = self.take_text() {
                self.queued = markup;
                return Ok(XmlEvent::Text(text));
            }
            if let Some(ev) = markup {
                return Ok(ev);
            }
        }
    }

    fn take_text(&mut self) -> Option<String> {
        if !self.have_text {
            return None;
        }
        self.have_text = false;
        let run = std::mem::take(&mut self.text);
        if !self.trim {
            return Some(run);
        }
        let trimmed = run.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r'));
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compact rendering of the event stream: `<a`, `<a/`, `a>`, `"text"`.
    fn dump(doc: &str, trim: bool) -> Result<Vec<String>, XmlError> {
        let mut ev = XmlEvents::new(doc, trim);
        let mut out = Vec::new();
        loop {
            match ev.next()? {
                XmlEvent::Start(e) => out.push(format!("<{}", e.name().into_inner())),
                XmlEvent::Empty(e) => out.push(format!("<{}/", e.name().into_inner())),
                XmlEvent::End(e) => out.push(format!("{}>", e.name().into_inner())),
                XmlEvent::Text(t) => out.push(format!("{t:?}")),
                XmlEvent::Eof => return Ok(out),
            }
        }
    }

    #[test]
    fn references_are_folded_into_one_text_run() {
        assert_eq!(
            dump("<a>AT&amp;T &lt;3 &#65;&#x42;&apos;&quot;&gt;</a>", false).unwrap(),
            vec!["<a", "\"AT&T <3 AB'\\\">\"", "a>"]
        );
    }

    #[test]
    fn whitespace_next_to_a_reference_survives_trimming() {
        assert_eq!(
            dump("<a>  x &amp; y  </a>", true).unwrap(),
            vec!["<a", "\"x & y\"", "a>"]
        );
        assert_eq!(
            dump("<a>  x &amp; y  </a>", false).unwrap(),
            vec!["<a", "\"  x & y  \"", "a>"]
        );
    }

    #[test]
    fn trim_drops_whitespace_only_runs() {
        assert_eq!(
            dump("<a>\n  <b>v</b>\n  <c/>\n</a>", true).unwrap(),
            vec!["<a", "<b", "\"v\"", "b>", "<c/", "a>"]
        );
        assert_eq!(
            dump("<a>\n  <b>v</b>\n</a>", false).unwrap(),
            vec!["<a", "\"\\n  \"", "<b", "\"v\"", "b>", "\"\\n\"", "a>"]
        );
    }

    #[test]
    fn markup_between_text_splits_runs() {
        // mixed content: one run per stretch between child elements
        assert_eq!(
            dump("<p>one <b>two</b> three</p>", false).unwrap(),
            vec!["<p", "\"one \"", "<b", "\"two\"", "b>", "\" three\"", "p>"]
        );
        // a comment ends a run too
        assert_eq!(
            dump("<p>one<!-- c -->two</p>", false).unwrap(),
            vec!["<p", "\"one\"", "\"two\"", "p>"]
        );
    }

    #[test]
    fn line_ends_are_normalised() {
        assert_eq!(
            dump("<a>l1\r\nl2\rl3</a>", false).unwrap(),
            vec!["<a", "\"l1\\nl2\\nl3\"", "a>"]
        );
    }

    #[test]
    fn declaration_doctype_and_pi_are_skipped() {
        assert_eq!(
            dump("<?xml version=\"1.0\"?><!DOCTYPE a><?pi x?><a>t</a>", false).unwrap(),
            vec!["<a", "\"t\"", "a>"]
        );
    }

    #[test]
    fn unknown_entity_is_an_error() {
        let err = dump("<a>&nbsp;</a>", false).unwrap_err().to_string();
        assert!(err.contains("&nbsp;"), "{err}");
    }

    #[test]
    fn malformed_markup_is_an_error() {
        assert!(dump("<a><b></a>", false).is_err());
    }
}
