use bytes::Bytes;
use futures::stream::TryStreamExt;
use futures::{Future, Stream};
use reqwest::{RequestBuilder, Response};
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::error::{BoxError, Error as GenaiError};

/// WebStream is a simple web stream implementation that splits the stream messages by a given delimiter.
/// - It is intended to be a pragmatic solution for services that do not adhere to the `text/event-stream` format and content type.
/// - For providers that support the standard `text/event-stream`, `genai` uses the `reqwest-eventsource`/`eventsource-stream` crates.
/// - This stream item is just a `String` and has different stream modes that define the message delimiter strategy (without any event typing).
/// - Each "Event" is just string-based and has only one event type, which is a string.
/// - It is the responsibility of the user of this stream to wrap it into a semantically correct stream of events depending on the domain.
#[allow(clippy::type_complexity)]
pub struct WebStream {
	stream_mode: StreamMode,
	reqwest_builder: Option<RequestBuilder>,
	response_future: Option<Pin<Box<dyn Future<Output = Result<Response, BoxError>> + Send>>>,
	bytes_stream: Option<Pin<Box<dyn Stream<Item = Result<Bytes, BoxError>> + Send>>>,
	// If a poll was a partial message, then we keep the previous part
	partial_message: Option<String>,
	// If a poll retrieved multiple messages, we keep them to be sent in the next poll
	remaining_messages: Option<VecDeque<String>>,
	// Raw bytes left over when an HTTP chunk ends mid-UTF-8-character. They are
	// prepended to the next chunk before decoding. This sits *below* the
	// `partial_message` delimiter buffering: it fixes byte-boundary splits, while
	// `partial_message` carries an incomplete delimited message.
	partial_bytes: Vec<u8>,
}

pub enum StreamMode {
	// This is used for Cohere with a single `\n`
	Delimiter(&'static str),
	// This is for Gemini (standard JSON array, pretty formatted)
	PrettyJsonArray,
}

impl WebStream {
	pub fn new_with_delimiter(reqwest_builder: RequestBuilder, message_delimiter: &'static str) -> Self {
		Self {
			stream_mode: StreamMode::Delimiter(message_delimiter),
			reqwest_builder: Some(reqwest_builder),
			response_future: None,
			bytes_stream: None,
			partial_message: None,
			remaining_messages: None,
			partial_bytes: Vec::new(),
		}
	}

	pub fn new_with_pretty_json_array(reqwest_builder: RequestBuilder) -> Self {
		Self {
			stream_mode: StreamMode::PrettyJsonArray,
			reqwest_builder: Some(reqwest_builder),
			response_future: None,
			bytes_stream: None,
			partial_message: None,
			remaining_messages: None,
			partial_bytes: Vec::new(),
		}
	}
}

impl Stream for WebStream {
	type Item = Result<String, BoxError>;

	fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
		let this = self.get_mut();

		// -- First, we check if we have any remaining messages to send.
		if let Some(ref mut remaining_messages) = this.remaining_messages
			&& let Some(msg) = remaining_messages.pop_front()
		{
			return Poll::Ready(Some(Ok(msg)));
		}

		// -- Then execute the web poll and processing loop
		loop {
			if let Some(ref mut fut) = this.response_future {
				match Pin::new(fut).poll(cx) {
					Poll::Ready(Ok(response)) => {
						// Check HTTP status before proceeding with the stream
						let status = response.status();
						if !status.is_success() {
							this.response_future = None;
							// For error responses, we need to read the body to get the error message
							// Store a future that reads the body and returns an error
							let error_future = async move {
								let body = response
									.text()
									.await
									.unwrap_or_else(|e| format!("Failed to read error body: {}", e));
								Err::<Response, BoxError>(Box::new(GenaiError::HttpError {
									status,
									canonical_reason: status.canonical_reason().unwrap_or("Unknown").to_string(),
									body,
								}))
							};
							this.response_future = Some(Box::pin(error_future));
							continue;
						}
						let bytes_stream = response.bytes_stream().map_err(|e| Box::new(e) as BoxError);
						this.bytes_stream = Some(Box::pin(bytes_stream));
						this.response_future = None;
					}
					Poll::Ready(Err(e)) => {
						this.response_future = None;
						return Poll::Ready(Some(Err(e)));
					}
					Poll::Pending => return Poll::Pending,
				}
			}

			if let Some(ref mut stream) = this.bytes_stream {
				match stream.as_mut().poll_next(cx) {
					Poll::Ready(Some(Ok(bytes))) => {
						// Decode the chunk, carrying any trailing bytes of a UTF-8
						// character that an HTTP/TLS chunk boundary split in half.
						let buff_string = match decode_chunk(&mut this.partial_bytes, &bytes) {
							Ok(s) => s,
							Err(e) => return Poll::Ready(Some(Err(Box::new(e) as BoxError))),
						};

						// The whole chunk was a single partial character — wait for more
						// bytes rather than feeding an empty string into the parser.
						if buff_string.is_empty() {
							continue;
						}

						// -- Iterate through the parts
						let buff_response = match this.stream_mode {
							StreamMode::Delimiter(delimiter) => {
								process_buff_string_delimited(buff_string, &mut this.partial_message, delimiter)
							}
							StreamMode::PrettyJsonArray => {
								new_with_pretty_json_array(buff_string, &mut this.partial_message)
							}
						};

						let BuffResponse {
							mut first_message,
							next_messages,
							candidate_message,
						} = buff_response?;

						// -- Add next_messages as remaining messages if present
						if let Some(next_messages) = next_messages {
							this.remaining_messages.get_or_insert(VecDeque::new()).extend(next_messages);
						}

						// -- If we still have a candidate, it's the partial for the next one
						if let Some(candidate_message) = candidate_message {
							// For now, we will just log this
							if this.partial_message.is_some() {
								tracing::warn!("GENAI - WARNING - partial_message is not none");
							}
							this.partial_message = Some(candidate_message);
						}

						// -- If we have a first message, we have to send it.
						if let Some(first_message) = first_message.take() {
							return Poll::Ready(Some(Ok(first_message)));
						} else {
							continue;
						}
					}
					Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
					Poll::Ready(None) => {
						// A well-formed stream ends on a complete character; leftover
						// bytes here mean the response was truncated mid-character.
						if !this.partial_bytes.is_empty() {
							tracing::debug!(
								"GENAI - WebStream ended with {} undecoded trailing byte(s)",
								this.partial_bytes.len()
							);
							this.partial_bytes.clear();
						}
						if let Some(partial) = this.partial_message.take()
							&& !partial.is_empty()
						{
							return Poll::Ready(Some(Ok(partial)));
						}
						this.bytes_stream = None;
					}
					Poll::Pending => return Poll::Pending,
				}
			}

			if let Some(reqwest_builder) = this.reqwest_builder.take() {
				let fut = async move { reqwest_builder.send().await.map_err(|e| Box::new(e) as BoxError) };
				this.response_future = Some(Box::pin(fut));
				continue;
			}

			return Poll::Ready(None);
		}
	}
}

/// Decode one raw HTTP chunk into UTF-8, carrying a character that a chunk
/// boundary split across `partial_bytes`.
///
/// - Any bytes left over from the previous call are prepended.
/// - A complete buffer decodes fully and leaves `partial_bytes` empty.
/// - A buffer ending mid-character returns the valid prefix and stashes the
///   incomplete tail in `partial_bytes` for the next call (may return `""`).
/// - Genuinely invalid bytes (not just a truncated tail) return `Err`.
fn decode_chunk(partial_bytes: &mut Vec<u8>, bytes: &[u8]) -> Result<String, std::str::Utf8Error> {
	// Fast path: nothing carried over — decode in place without an extra alloc.
	if partial_bytes.is_empty() {
		return match std::str::from_utf8(bytes) {
			Ok(s) => Ok(s.to_string()),
			Err(e) => match e.error_len() {
				None => {
					let valid = e.valid_up_to();
					*partial_bytes = bytes[valid..].to_vec();
					// `valid_up_to()` guarantees this prefix is valid UTF-8.
					Ok(std::str::from_utf8(&bytes[..valid])
						.expect("valid_up_to bytes are valid UTF-8")
						.to_string())
				}
				Some(_) => Err(e),
			},
		};
	}

	let mut buf = std::mem::take(partial_bytes);
	buf.extend_from_slice(bytes);
	match std::str::from_utf8(&buf) {
		Ok(s) => Ok(s.to_string()),
		Err(e) => match e.error_len() {
			None => {
				let valid = e.valid_up_to();
				// `valid_up_to()` guarantees this prefix is valid UTF-8.
				let s = std::str::from_utf8(&buf[..valid])
					.expect("valid_up_to bytes are valid UTF-8")
					.to_string();
				*partial_bytes = buf[valid..].to_vec();
				Ok(s)
			}
			Some(_) => Err(e),
		},
	}
}

struct BuffResponse {
	first_message: Option<String>,
	next_messages: Option<Vec<String>>,
	candidate_message: Option<String>,
}

/// Process a string buffer for the pretty_json_array (for Gemini)
/// It will split the messages as follows:
/// - If it starts with `[`, then the message will be `[`
/// - Then, each main JSON object (from the first `{` to the last `}`) will become a message
/// - Main JSON object `,` delimiter will be skipped
/// - The ending `]` will be sent as a `]` message as well.
///
/// IMPORTANT: Right now, it assumes each buff_string will contain the full main JSON object
///            for each array item (which seems to be the case with Gemini).
///            This probably needs to be made more robust later.
fn new_with_pretty_json_array(
	buff_string: String,
	partial_message: &mut Option<String>,
) -> Result<BuffResponse, crate::webc::Error> {
	let mut buff_str = buff_string.as_str();

	let mut messages: Vec<String> = Vec::new();

	// -- 1. Prepend partial message if any
	let full_string_holder: String;
	if let Some(partial) = partial_message.take() {
		full_string_holder = format!("{}{}", partial, buff_str);
		buff_str = full_string_holder.as_str();
	}

	// -- 2. Process the buffer
	// We want to extract valid JSON objects.
	// The stream is expected to be: `[` (optional), `{...}`, `,`, `{...}`, `]` (optional)
	// We need to be robust against whitespace and commas.

	let mut depth = 0;
	let mut in_string = false;
	let mut escape = false;
	let mut start_idx = 0;
	let mut last_idx = 0; // Track the end of the last processed object

	for (idx, c) in buff_str.char_indices() {
		if in_string {
			if escape {
				escape = false;
			} else if c == '\\' {
				escape = true;
			} else if c == '"' {
				in_string = false;
			}
		} else {
			match c {
				'"' => in_string = true,
				'{' => {
					if depth == 0 {
						start_idx = idx;
					}
					depth += 1;
				}
				'}' => {
					depth -= 1;
					if depth == 0 {
						// Found a complete JSON object
						// idx is the byte index of '}'. We want to include it.
						// '}' is 1 byte, so end range is idx + 1
						let json_str = &buff_str[start_idx..idx + 1];

						// Verify it's valid JSON (optional but good for safety)
						if serde_json::from_str::<serde_json::Value>(json_str).is_ok() {
							messages.push(json_str.to_string());
						} else {
							// Should not happen if logic is correct
							tracing::warn!("WebStream: Extracted block failed JSON validation: {}", json_str);
						}
						// Update last_idx to point after this object
						last_idx = idx + 1;
					}
				}
				'[' => {
					if depth == 0 {
						messages.push("[".to_string());
						last_idx = idx + 1;
					}
				}
				']' => {
					if depth == 0 {
						messages.push("]".to_string());
						last_idx = idx + 1;
					}
				}
				_ => {
					// Ignore other characters outside of objects (whitespace, commas)
				}
			}
		}
	}

	// -- 3. Handle remaining partial
	// last_idx points to the byte after the last successfully processed object/token
	if last_idx < buff_str.len() {
		let remaining = &buff_str[last_idx..];
		if !remaining.trim().is_empty() {
			*partial_message = Some(remaining.to_string());
		}
	}

	// -- Return the buff response
	let first_message = if !messages.is_empty() {
		Some(messages[0].to_string())
	} else {
		None
	};

	let next_messages = if messages.len() > 1 {
		Some(messages[1..].to_vec())
	} else {
		None
	};

	Ok(BuffResponse {
		first_message,
		next_messages,
		candidate_message: partial_message.take(),
	})
}

/// Process a string buffer for the delimited mode (e.g., Cohere)
fn process_buff_string_delimited(
	buff_string: String,
	partial_message: &mut Option<String>,
	delimiter: &str,
) -> Result<BuffResponse, crate::webc::Error> {
	let full_string = if let Some(partial) = partial_message.take() {
		format!("{partial}{buff_string}")
	} else {
		buff_string
	};

	let mut parts: Vec<String> = full_string.split(delimiter).map(|s| s.to_string()).collect();

	// The last part is the new partial (what's after the last delimiter)
	let candidate_message = parts.pop();

	// Filter out empty strings that result from multiple delimiters (e.g., \n\n\n\n)
	let mut messages: Vec<String> = parts.into_iter().filter(|s| !s.is_empty()).collect();

	let mut first_message = None;
	let mut next_messages = None;

	if !messages.is_empty() {
		first_message = Some(messages.remove(0));
		if !messages.is_empty() {
			next_messages = Some(messages);
		}
	}

	Ok(BuffResponse {
		first_message,
		next_messages,
		candidate_message,
	})
}

#[cfg(test)]
mod tests {
	use super::decode_chunk;

	/// A 2-byte Cyrillic char (U+0444 "ф" = 0xD1 0x84) split across two chunks
	/// must not error — the valid prefix decodes and the tail carries over.
	#[test]
	fn carries_split_two_byte_char() {
		let full = "аф".as_bytes(); // U+0430 (2B) + U+0444 (2B)
		let split = full.len() - 1; // break inside the last char
		let mut partial = Vec::new();

		let first = decode_chunk(&mut partial, &full[..split]).unwrap();
		assert_eq!(first, "а");
		assert_eq!(partial.len(), 1); // one trailing byte held back

		let second = decode_chunk(&mut partial, &full[split..]).unwrap();
		assert_eq!(second, "ф");
		assert!(partial.is_empty());
	}

	/// A 4-byte emoji (U+1F600) split one byte at a time still reassembles.
	#[test]
	fn carries_split_four_byte_char_byte_by_byte() {
		let emoji = "😀".as_bytes();
		assert_eq!(emoji.len(), 4);
		let mut partial = Vec::new();
		let mut out = String::new();
		for b in emoji {
			out.push_str(&decode_chunk(&mut partial, &[*b]).unwrap());
		}
		assert_eq!(out, "😀");
		assert!(partial.is_empty());
	}

	/// Whole, valid chunk with an empty carry takes the fast path unchanged.
	#[test]
	fn whole_chunk_fast_path() {
		let mut partial = Vec::new();
		assert_eq!(decode_chunk(&mut partial, b"hello").unwrap(), "hello");
		assert!(partial.is_empty());
	}

	/// Genuinely invalid bytes (0xFF) are surfaced as an error, not buffered.
	#[test]
	fn rejects_invalid_bytes() {
		let mut partial = Vec::new();
		assert!(decode_chunk(&mut partial, &[0x68, 0xFF, 0x69]).is_err());
	}
}
