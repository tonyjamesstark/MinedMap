//! Repair of NBT strings that fastnbt refuses to decode
//!
//! Java writes strings as modified UTF-8, which can hold unpaired UTF-16
//! surrogates (for example a sign line cut off in the middle of an emoji).
//! fastnbt rejects those strings, failing the whole chunk.

/// Replaces unpaired surrogates in all strings of an NBT buffer with U+FFFD
///
/// Both are 3 bytes long in modified UTF-8, so no length prefix changes.
/// Returns whether anything was replaced, or `None` if the buffer is not
/// well-formed NBT.
pub fn repair_strings(buf: &mut [u8]) -> Option<bool> {
	let mut walker = Walker {
		buf,
		pos: 0,
		changed: false,
	};
	let tag = walker.byte()?;
	walker.string()?;
	walker.payload(tag, 0)?;
	Some(walker.changed)
}

/// Deepest list/compound nesting followed before giving up
const MAX_DEPTH: usize = 512;

/// Cursor over an NBT buffer
struct Walker<'a> {
	/// The buffer being repaired
	buf: &'a mut [u8],
	/// Current read position
	pos: usize,
	/// Whether a string was changed
	changed: bool,
}

impl Walker<'_> {
	/// Advances by `len` bytes and returns the skipped range
	fn take(&mut self, len: usize) -> Option<std::ops::Range<usize>> {
		let start = self.pos;
		let end = start
			.checked_add(len)
			.filter(|&end| end <= self.buf.len())?;
		self.pos = end;
		Some(start..end)
	}

	/// Reads one byte
	fn byte(&mut self) -> Option<u8> {
		let range = self.take(1)?;
		Some(self.buf[range.start])
	}

	/// Reads a big-endian array length
	fn len(&mut self) -> Option<usize> {
		let range = self.take(4)?;
		let len = i32::from_be_bytes(self.buf[range].try_into().ok()?);
		usize::try_from(len.max(0)).ok()
	}

	/// Skips `count` elements of `size` bytes
	fn skip(&mut self, count: usize, size: usize) -> Option<()> {
		self.take(count.checked_mul(size)?).map(|_| ())
	}

	/// Reads a string and repairs it in place
	fn string(&mut self) -> Option<()> {
		let range = self.take(2)?;
		let len = u16::from_be_bytes(self.buf[range].try_into().ok()?);
		let range = self.take(len.into())?;
		self.changed |= repair_surrogates(&mut self.buf[range]);
		Some(())
	}

	/// Walks the payload of a tag of the given type
	fn payload(&mut self, tag: u8, depth: usize) -> Option<()> {
		if depth > MAX_DEPTH {
			return None;
		}
		match tag {
			1 => self.skip(1, 1),
			2 => self.skip(1, 2),
			3 | 5 => self.skip(1, 4),
			4 | 6 => self.skip(1, 8),
			7 => {
				let len = self.len()?;
				self.skip(len, 1)
			}
			8 => self.string(),
			9 => {
				let inner = self.byte()?;
				let len = self.len()?;
				if inner == 0 {
					return Some(());
				}
				for _ in 0..len {
					self.payload(inner, depth + 1)?;
				}
				Some(())
			}
			10 => loop {
				let inner = self.byte()?;
				if inner == 0 {
					return Some(());
				}
				self.string()?;
				self.payload(inner, depth + 1)?;
			},
			11 => {
				let len = self.len()?;
				self.skip(len, 4)
			}
			12 => {
				let len = self.len()?;
				self.skip(len, 8)
			}
			_ => None,
		}
	}
}

/// Replaces unpaired surrogates in modified UTF-8 with U+FFFD
fn repair_surrogates(s: &mut [u8]) -> bool {
	const REPLACEMENT: [u8; 3] = [0xEF, 0xBF, 0xBD];
	let is_surrogate = |s: &[u8], i: usize, range: std::ops::RangeInclusive<u8>| {
		s.len() >= i + 3 && s[i] == 0xED && range.contains(&s[i + 1])
	};

	let mut changed = false;
	let mut i = 0;
	while i < s.len() {
		if is_surrogate(s, i, 0xA0..=0xAF) && is_surrogate(s, i + 3, 0xB0..=0xBF) {
			i += 6;
		} else if is_surrogate(s, i, 0xA0..=0xBF) {
			s[i..i + 3].copy_from_slice(&REPLACEMENT);
			changed = true;
			i += 3;
		} else {
			i += 1;
		}
	}
	changed
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Builds `{name: <list of strings>}` with raw string bytes
	fn compound(strings: &[&[u8]]) -> Vec<u8> {
		let mut buf = vec![10, 0, 0, 9, 0, 1, b'm', 8];
		buf.extend((strings.len() as i32).to_be_bytes());
		for s in strings {
			buf.extend((s.len() as u16).to_be_bytes());
			buf.extend(*s);
		}
		buf.extend([12, 0, 1, b'l', 0, 0, 0, 1, 0xED, 0xA0, 0xB5, 0, 0, 0, 0, 0]);
		buf.push(0);
		buf
	}

	#[test]
	fn replaces_unpaired_surrogates() {
		let devil = cesu8_devil();
		let lone_high = [b'a', 0xED, 0xA0, 0xB5];
		let lone_low = [0xED, 0xB0, 0x80, b'b'];
		let mut buf = compound(&[&devil, &lone_high, &lone_low]);
		assert_eq!(fastnbt::from_bytes::<fastnbt::Value>(&buf).ok(), None);

		assert_eq!(repair_strings(&mut buf), Some(true));
		let value: fastnbt::Value = fastnbt::from_bytes(&buf).unwrap();
		let fastnbt::Value::Compound(root) = value else {
			panic!("{value:?}")
		};
		let strings: Vec<_> = match &root["m"] {
			fastnbt::Value::List(list) => list.iter().map(|v| v.as_str().unwrap()).collect(),
			other => panic!("{other:?}"),
		};
		assert_eq!(strings, ["😈", "a\u{FFFD}", "\u{FFFD}b"]);
	}

	#[test]
	fn leaves_long_arrays_alone() {
		let mut buf = compound(&[b"ok"]);
		let before = buf.clone();
		assert_eq!(repair_strings(&mut buf), Some(false));
		assert_eq!(buf, before);
	}

	#[test]
	fn rejects_truncated_data() {
		let mut buf = compound(&[b"ok"]);
		buf.truncate(buf.len() - 4);
		assert_eq!(repair_strings(&mut buf), None);
	}

	/// U+1F608 as a surrogate pair in modified UTF-8
	fn cesu8_devil() -> Vec<u8> {
		vec![0xED, 0xA0, 0xBD, 0xED, 0xB8, 0x88]
	}
}
