//! Shared Ködade CLI socket protocol.
//!
//! Each JSON message is UTF-8 and terminated by one newline. Message payloads
//! that contain byte streams use serde's JSON byte-array representation.

use anyhow::Result;
use serde::{de::DeserializeOwned, Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello { cols: u16, rows: u16 },
    Input { bytes: Vec<u8> },
    Resize { cols: u16, rows: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    Welcome { session: String },
    Screen(Screen),
    Error { message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Screen {
    pub contents: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>> {
    let mut encoded = serde_json::to_vec(message)?;
    encoded.push(b'\n');
    Ok(encoded)
}

pub fn decode<T: DeserializeOwned>(line: &[u8]) -> Result<T> {
    Ok(serde_json::from_slice(line)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_as_newline_delimited_json() {
        let message = ClientMessage::Input {
            bytes: vec![0, b'a', 255],
        };

        let encoded = encode(&message).expect("message encodes");
        assert_eq!(encoded.last(), Some(&b'\n'));
        assert_eq!(
            decode::<ClientMessage>(&encoded).expect("message decodes"),
            message
        );
    }
}
