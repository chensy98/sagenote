//! Newline-delimited JSON frames, Jupyter-shaped.
//!
//! One JSON object per line. A JSON line cannot contain a raw newline, so no
//! delimiter sentinel is needed and user data cannot collide with framing.
//! `type` is the tag; ids are opaque to the driver and echoed verbatim.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{CellId, IntrospectId, MimeType, RunId};

/// Where a stream chunk came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StreamName {
    Stdout,
    Stderr,
}

impl StreamName {
    pub fn as_str(self) -> &'static str {
        match self {
            StreamName::Stdout => "stdout",
            StreamName::Stderr => "stderr",
        }
    }
}

/// Block vs inline comes from the driver. The renderer must never guess it from
/// the LaTeX string (a `egin{...}` is a hint, not a contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DisplayMode {
    #[default]
    Block,
    Inline,
}

impl DisplayMode {
    pub fn as_str(self) -> &'static str {
        match self {
            DisplayMode::Block => "block",
            DisplayMode::Inline => "inline",
        }
    }
}

/// A mimetype-keyed output bundle. Unknown keys are preserved so a future
/// backend can add `application/x-...` without breaking old clients.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MimeBundle(pub BTreeMap<String, String>);

impl MimeBundle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, mime: MimeType, data: impl Into<String>) -> Option<String> {
        self.0.insert(mime.as_str().to_owned(), data.into())
    }

    pub fn insert_raw(
        &mut self,
        mime: impl Into<String>,
        data: impl Into<String>,
    ) -> Option<String> {
        self.0.insert(mime.into(), data.into())
    }

    pub fn get(&self, mime: MimeType) -> Option<&str> {
        self.0.get(mime.as_str()).map(String::as_str)
    }

    pub fn contains(&self, mime: MimeType) -> bool {
        self.0.contains_key(mime.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VarInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub type_name: String,
    pub value: String,
}

/// One protocol frame.
///
/// `#[serde(other)]` makes a newer driver's frame non-fatal: the supervisor
/// logs it and keeps the session. Cell results are always typed; this is not
/// text sniffing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    Ready {
        sage_version: String,
    },
    Start {
        id: CellId,
        run: RunId,
    },
    Stream {
        name: StreamName,
        text: String,
    },
    Result {
        id: CellId,
        run: RunId,
        bundle: MimeBundle,
        #[serde(default)]
        display: DisplayMode,
    },
    Error {
        id: CellId,
        run: RunId,
        traceback: String,
    },
    Introspect {
        id: IntrospectId,
        vars: Vec<VarInfo>,
    },
    End {
        id: CellId,
        run: RunId,
    },
    /// Auxiliary liveness answer to stdin `{"op":"ping"}`. It is never part of
    /// a cell's output stream.
    Pong,
    #[serde(other)]
    Unknown,
}

impl Frame {
    pub fn from_json_line(line: &str) -> serde_json::Result<Self> {
        serde_json::from_str(line.trim_end_matches(['\n', '\r']))
    }

    pub fn to_json_line(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }

    /// (cell, run) for frames that belong to one evaluation.
    pub fn cell_run(&self) -> Option<(CellId, RunId)> {
        match self {
            Frame::Start { id, run }
            | Frame::Result { id, run, .. }
            | Frame::Error { id, run, .. }
            | Frame::End { id, run } => Some((*id, *run)),
            _ => None,
        }
    }
}

/// Input side of the protocol (Rust -> driver), one object per line.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DriverRequest<'a> {
    Eval {
        id: CellId,
        run: RunId,
        source: &'a str,
    },
    Introspect {
        id: IntrospectId,
    },
    SetDisplay {
        mode: DisplayMode,
    },
    Ping,
}

impl DriverRequest<'_> {
    pub fn to_json_line(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> CellId {
        CellId(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn ready_roundtrip() {
        let f = Frame::Ready {
            sage_version: "10.10".into(),
        };
        let line = f.to_json_line().unwrap();
        assert_eq!(line, r#"{"type":"ready","sage_version":"10.10"}"#);
        assert_eq!(Frame::from_json_line(&line).unwrap(), f);
    }

    #[test]
    fn result_bundle_roundtrip() {
        let mut bundle = MimeBundle::new();
        bundle.insert(MimeType::TextLatex, r"\frac{1}{2}");
        bundle.insert_raw("application/x-test", "kept");
        let f = Frame::Result {
            id: id(1),
            run: RunId(7),
            bundle,
            display: DisplayMode::Inline,
        };
        let line = f.to_json_line().unwrap();
        let back = Frame::from_json_line(&line).unwrap();
        assert_eq!(back, f);
        if let Frame::Result {
            bundle, display, ..
        } = back
        {
            assert_eq!(bundle.get(MimeType::TextLatex), Some(r"\frac{1}{2}"));
            assert_eq!(bundle.iter().count(), 2);
            assert_eq!(display, DisplayMode::Inline);
        } else {
            panic!("wrong frame");
        }
    }

    #[test]
    fn ids_are_opaque_json_strings() {
        let f = Frame::End {
            id: id(0xabc),
            run: RunId(9),
        };
        let line = f.to_json_line().unwrap();
        assert!(line.contains("00000000-0000-0000-0000-000000000abc"));
        assert_eq!(Frame::from_json_line(&line).unwrap(), f);
    }

    #[test]
    fn unknown_frame_is_kept_as_unknown_not_fatal() {
        let f = Frame::from_json_line(r#"{"type":"future_thing","payload":42}"#).unwrap();
        assert_eq!(f, Frame::Unknown);
    }

    #[test]
    fn display_defaults_to_block() {
        let f = Frame::from_json_line(
            r#"{"type":"result","id":"00000000-0000-0000-0000-000000000001","run":1,"bundle":{"text/plain":"x"}}"#,
        )
        .unwrap();
        match f {
            Frame::Result { display, .. } => assert_eq!(display, DisplayMode::Block),
            _ => panic!("wrong frame"),
        }
    }

    #[test]
    fn stream_and_introspect() {
        let s = Frame::Stream {
            name: StreamName::Stderr,
            text: "warning\n".into(),
        };
        assert_eq!(
            s.to_json_line().unwrap(),
            r#"{"type":"stream","name":"stderr","text":"warning\n"}"#
        );
        let i = Frame::Introspect {
            id: IntrospectId(3),
            vars: vec![VarInfo {
                name: "x".into(),
                type_name: "Integer".into(),
                value: "42".into(),
            }],
        };
        let line = i.to_json_line().unwrap();
        assert!(line.contains(r#""type":"introspect""#));
        assert_eq!(Frame::from_json_line(&line).unwrap(), i);
    }

    #[test]
    fn request_encoding() {
        let r = DriverRequest::Eval {
            id: id(2),
            run: RunId(3),
            source: "1+1",
        };
        assert!(r.to_json_line().unwrap().contains(r#""op":"eval""#));
        assert_eq!(
            DriverRequest::Ping.to_json_line().unwrap(),
            r#"{"op":"ping"}"#
        );
        assert_eq!(
            DriverRequest::SetDisplay {
                mode: DisplayMode::Inline
            }
            .to_json_line()
            .unwrap(),
            r#"{"op":"set_display","mode":"inline"}"#
        );
    }
}
