use crate::error::{Result, ServerError};
use crate::packstream::Value;

// -- Bolt structure tags ---------------------------------------------

// client messages
const TAG_HELLO: u8 = 0x01;
const TAG_LOGON: u8 = 0x6A;
const TAG_LOGOFF: u8 = 0x6B;
const TAG_RUN: u8 = 0x10;
const TAG_PULL: u8 = 0x3F;
const TAG_DISCARD: u8 = 0x2F;
const TAG_BEGIN: u8 = 0x11;
const TAG_COMMIT: u8 = 0x12;
const TAG_ROLLBACK: u8 = 0x13;
const TAG_RESET: u8 = 0x0F;
const TAG_GOODBYE: u8 = 0x02;
const TAG_ROUTE: u8 = 0x66;
const TAG_TELEMETRY: u8 = 0x54;

// server messages
const TAG_SUCCESS: u8 = 0x70;
const TAG_FAILURE: u8 = 0x7F;
const TAG_IGNORED: u8 = 0x7E;
const TAG_RECORD: u8 = 0x71;

/// Client -> server messages.
#[derive(Debug)]
pub enum ClientMessage {
    Hello {
        extra: Vec<(String, Value)>,
    },
    Logon {
        auth: Vec<(String, Value)>,
    },
    Logoff,
    Run {
        query: String,
        parameters: Vec<(String, Value)>,
        extra: Vec<(String, Value)>,
    },
    Pull {
        extra: Vec<(String, Value)>,
    },
    Discard {
        extra: Vec<(String, Value)>,
    },
    Begin {
        extra: Vec<(String, Value)>,
    },
    Commit,
    Rollback,
    Reset,
    Goodbye,
    Route {
        routing: Vec<(String, Value)>,
        bookmarks: Vec<String>,
        db: Option<String>,
    },
    Telemetry {
        api: i64,
    },
}

/// Server -> client messages.
#[derive(Debug)]
pub enum ServerMessage {
    Success { metadata: Vec<(String, Value)> },
    Failure { metadata: Vec<(String, Value)> },
    Ignored,
    Record { fields: Vec<Value> },
}

/// PackStream structure -> ClientMessage.
pub fn decode_client_message(value: Value) -> Result<ClientMessage> {
    match value {
        Value::Structure { tag, mut fields } => match tag {
            TAG_HELLO => {
                let extra = take_map(&mut fields, 0)?;
                Ok(ClientMessage::Hello { extra })
            }
            TAG_LOGON => {
                let auth = take_map(&mut fields, 0)?;
                Ok(ClientMessage::Logon { auth })
            }
            TAG_LOGOFF => Ok(ClientMessage::Logoff),
            TAG_RUN => {
                let query = take_string(&mut fields, 0)?;
                let parameters = if fields.len() > 1 {
                    take_map(&mut fields, 1)?
                } else {
                    vec![]
                };
                let extra = if fields.len() > 2 {
                    take_map(&mut fields, 2)?
                } else {
                    vec![]
                };
                Ok(ClientMessage::Run {
                    query,
                    parameters,
                    extra,
                })
            }
            TAG_PULL => {
                let extra = if fields.is_empty() {
                    vec![]
                } else {
                    take_map(&mut fields, 0)?
                };
                Ok(ClientMessage::Pull { extra })
            }
            TAG_DISCARD => {
                let extra = if fields.is_empty() {
                    vec![]
                } else {
                    take_map(&mut fields, 0)?
                };
                Ok(ClientMessage::Discard { extra })
            }
            TAG_BEGIN => {
                let extra = if fields.is_empty() {
                    vec![]
                } else {
                    take_map(&mut fields, 0)?
                };
                Ok(ClientMessage::Begin { extra })
            }
            TAG_COMMIT => Ok(ClientMessage::Commit),
            TAG_ROLLBACK => Ok(ClientMessage::Rollback),
            TAG_RESET => Ok(ClientMessage::Reset),
            TAG_GOODBYE => Ok(ClientMessage::Goodbye),
            TAG_ROUTE => {
                let routing = take_map(&mut fields, 0)?;
                let bookmarks = if fields.len() > 1 {
                    match &fields[1] {
                        Value::List(items) => items
                            .iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect(),
                        _ => vec![],
                    }
                } else {
                    vec![]
                };
                let db = if fields.len() > 2 {
                    fields[2].as_str().map(String::from)
                } else {
                    None
                };
                Ok(ClientMessage::Route {
                    routing,
                    bookmarks,
                    db,
                })
            }
            TAG_TELEMETRY => {
                let api = if fields.is_empty() {
                    0
                } else {
                    fields[0].as_i64().unwrap_or(0)
                };
                Ok(ClientMessage::Telemetry { api })
            }
            _ => Err(ServerError::Protocol(format!(
                "unknown message tag: 0x{tag:02X}"
            ))),
        },
        _ => Err(ServerError::Protocol(
            "expected structure for client message".into(),
        )),
    }
}

/// ServerMessage -> PackStream Value (consumes the message).
pub fn encode_server_message(msg: ServerMessage) -> Value {
    match msg {
        ServerMessage::Success { metadata } => Value::Structure {
            tag: TAG_SUCCESS,
            fields: vec![Value::Map(metadata)],
        },
        ServerMessage::Failure { metadata } => Value::Structure {
            tag: TAG_FAILURE,
            fields: vec![Value::Map(metadata)],
        },
        ServerMessage::Ignored => Value::Structure {
            tag: TAG_IGNORED,
            fields: vec![],
        },
        ServerMessage::Record { fields } => Value::Structure {
            tag: TAG_RECORD,
            fields: vec![Value::List(fields)],
        },
    }
}

/// ClientMessage -> PackStream Value (for clients).
pub fn encode_client_message(msg: &ClientMessage) -> Value {
    match msg {
        ClientMessage::Hello { extra } => Value::Structure {
            tag: TAG_HELLO,
            fields: vec![Value::Map(extra.clone())],
        },
        ClientMessage::Logon { auth } => Value::Structure {
            tag: TAG_LOGON,
            fields: vec![Value::Map(auth.clone())],
        },
        ClientMessage::Logoff => Value::Structure {
            tag: TAG_LOGOFF,
            fields: vec![],
        },
        ClientMessage::Run {
            query,
            parameters,
            extra,
        } => Value::Structure {
            tag: TAG_RUN,
            fields: vec![
                Value::String(query.clone()),
                Value::Map(parameters.clone()),
                Value::Map(extra.clone()),
            ],
        },
        ClientMessage::Pull { extra } => Value::Structure {
            tag: TAG_PULL,
            fields: vec![Value::Map(extra.clone())],
        },
        ClientMessage::Discard { extra } => Value::Structure {
            tag: TAG_DISCARD,
            fields: vec![Value::Map(extra.clone())],
        },
        ClientMessage::Begin { extra } => Value::Structure {
            tag: TAG_BEGIN,
            fields: vec![Value::Map(extra.clone())],
        },
        ClientMessage::Commit => Value::Structure {
            tag: TAG_COMMIT,
            fields: vec![],
        },
        ClientMessage::Rollback => Value::Structure {
            tag: TAG_ROLLBACK,
            fields: vec![],
        },
        ClientMessage::Reset => Value::Structure {
            tag: TAG_RESET,
            fields: vec![],
        },
        ClientMessage::Goodbye => Value::Structure {
            tag: TAG_GOODBYE,
            fields: vec![],
        },
        ClientMessage::Route {
            routing,
            bookmarks,
            db,
        } => {
            let bookmarks_val =
                Value::List(bookmarks.iter().map(|b| Value::String(b.clone())).collect());
            let db_val = match db {
                Some(d) => Value::String(d.clone()),
                None => Value::Null,
            };
            Value::Structure {
                tag: TAG_ROUTE,
                fields: vec![Value::Map(routing.clone()), bookmarks_val, db_val],
            }
        }
        ClientMessage::Telemetry { api } => Value::Structure {
            tag: TAG_TELEMETRY,
            fields: vec![Value::Integer(*api)],
        },
    }
}

/// PackStream structure -> ServerMessage (for clients).
pub fn decode_server_message(value: Value) -> Result<ServerMessage> {
    match value {
        Value::Structure { tag, mut fields } => match tag {
            TAG_SUCCESS => {
                let metadata = take_map(&mut fields, 0)?;
                Ok(ServerMessage::Success { metadata })
            }
            TAG_FAILURE => {
                let metadata = take_map(&mut fields, 0)?;
                Ok(ServerMessage::Failure { metadata })
            }
            TAG_IGNORED => Ok(ServerMessage::Ignored),
            TAG_RECORD => {
                let fields_list = if fields.is_empty() {
                    vec![]
                } else {
                    match std::mem::replace(&mut fields[0], Value::Null) {
                        Value::List(l) => l,
                        other => vec![other],
                    }
                };
                Ok(ServerMessage::Record {
                    fields: fields_list,
                })
            }
            _ => Err(ServerError::Protocol(format!(
                "unknown server message tag: 0x{tag:02X}"
            ))),
        },
        _ => Err(ServerError::Protocol(
            "expected structure for server message".into(),
        )),
    }
}

fn take_string(fields: &mut [Value], idx: usize) -> Result<String> {
    if idx >= fields.len() {
        return Err(ServerError::Protocol("missing field".into()));
    }
    match std::mem::replace(&mut fields[idx], Value::Null) {
        Value::String(s) => Ok(s),
        other => Err(ServerError::Protocol(format!(
            "expected string at field {idx}, got {other:?}"
        ))),
    }
}

fn take_map(fields: &mut [Value], idx: usize) -> Result<Vec<(String, Value)>> {
    if idx >= fields.len() {
        return Ok(vec![]);
    }
    match std::mem::replace(&mut fields[idx], Value::Null) {
        Value::Map(m) => Ok(m),
        Value::Null => Ok(vec![]),
        other => Err(ServerError::Protocol(format!(
            "expected map at field {idx}, got {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packstream::{decode_value, encode_value};

    #[test]
    fn roundtrip_success() {
        let msg = ServerMessage::Success {
            metadata: vec![("server".into(), Value::String("Traverse/0.1".into()))],
        };
        let value = encode_server_message(msg);
        let mut buf = Vec::new();
        encode_value(&mut buf, &value);
        let mut pos = 0;
        let decoded = decode_value(&buf, &mut pos).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn roundtrip_record() {
        let msg = ServerMessage::Record {
            fields: vec![Value::Integer(42), Value::String("hello".into())],
        };
        let value = encode_server_message(msg);
        let mut buf = Vec::new();
        encode_value(&mut buf, &value);
        let mut pos = 0;
        let decoded = decode_value(&buf, &mut pos).unwrap();
        assert_eq!(decoded, value);
    }

    #[test]
    fn decode_hello() {
        let value = Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test/1.0".into()),
            )])],
        };
        let msg = decode_client_message(value).unwrap();
        match msg {
            ClientMessage::Hello { extra } => {
                assert_eq!(extra.len(), 1);
                assert_eq!(extra[0].0, "user_agent");
            }
            _ => panic!("expected Hello"),
        }
    }

    #[test]
    fn roundtrip_client_hello() {
        let msg = ClientMessage::Hello {
            extra: vec![("user_agent".into(), Value::String("test/1.0".into()))],
        };
        let value = encode_client_message(&msg);
        let decoded = decode_client_message(value).unwrap();
        match decoded {
            ClientMessage::Hello { extra } => {
                assert_eq!(extra.len(), 1);
                assert_eq!(extra[0].0, "user_agent");
            }
            _ => panic!("expected Hello"),
        }
    }

    #[test]
    fn roundtrip_client_run() {
        let msg = ClientMessage::Run {
            query: "RETURN 1".into(),
            parameters: vec![],
            extra: vec![],
        };
        let value = encode_client_message(&msg);
        let decoded = decode_client_message(value).unwrap();
        match decoded {
            ClientMessage::Run { query, .. } => assert_eq!(query, "RETURN 1"),
            _ => panic!("expected Run"),
        }
    }

    #[test]
    fn decode_server_success() {
        let msg = ServerMessage::Success {
            metadata: vec![("server".into(), Value::String("Traverse/0.1".into()))],
        };
        let value = encode_server_message(msg);
        let decoded = decode_server_message(value).unwrap();
        match decoded {
            ServerMessage::Success { metadata } => {
                assert_eq!(metadata.len(), 1);
                assert_eq!(metadata[0].0, "server");
            }
            _ => panic!("expected Success"),
        }
    }

    #[test]
    fn decode_server_record() {
        let msg = ServerMessage::Record {
            fields: vec![Value::Integer(42), Value::String("hello".into())],
        };
        let value = encode_server_message(msg);
        let decoded = decode_server_message(value).unwrap();
        match decoded {
            ServerMessage::Record { fields } => {
                assert_eq!(fields.len(), 2);
                assert_eq!(fields[0], Value::Integer(42));
            }
            _ => panic!("expected Record"),
        }
    }

    #[test]
    fn decode_run() {
        let value = Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("RETURN 1".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        };
        let msg = decode_client_message(value).unwrap();
        match msg {
            ClientMessage::Run { query, .. } => {
                assert_eq!(query, "RETURN 1");
            }
            _ => panic!("expected Run"),
        }
    }
}
