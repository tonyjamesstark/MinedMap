//! Functions for reading and deserializing region data

use std::{
	fs::File,
	io::{SeekFrom, prelude::*},
	path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use flate2::read::ZlibDecoder;
use serde::de::DeserializeOwned;

use minedmap_types::*;

/// Data block size of region data files
///
/// After one header block, the region file consists of one or more consecutive blocks
/// of data for each populated chunk.
const BLOCKSIZE: usize = 4096;

/// Chunk descriptor extracted from region file header
#[derive(Debug)]
struct ChunkDesc {
	/// Offset of data block where the chunk starts
	offset: u32,
	/// Number of data block used by the chunk
	len: u8,
	/// Coodinates of chunk described by this descriptor
	coords: ChunkCoords,
}

/// Parses the header of a region data file
fn parse_header(header: &ChunkArray<u32>) -> Vec<ChunkDesc> {
	let mut chunks: Vec<_> = header
		.iter()
		.filter_map(|(coords, &chunk)| {
			let offset_len = u32::from_be(chunk);

			let offset = offset_len >> 8;
			let len = offset_len as u8;

			if offset == 0 || len == 0 {
				return None;
			}

			Some(ChunkDesc {
				offset,
				len,
				coords,
			})
		})
		.collect();

	chunks.sort_by_key(|chunk| chunk.offset);

	chunks
}

/// Decompresses chunk data and deserializes to a given data structure
fn decode_chunk<T>(buf: &[u8]) -> Result<T>
where
	T: DeserializeOwned,
{
	let (format, buf) = buf.split_at(1);
	if format[0] != 2 {
		bail!("Unknown chunk format");
	}

	let mut decoder = ZlibDecoder::new(buf);
	let mut decode_buffer = vec![];
	decoder
		.read_to_end(&mut decode_buffer)
		.context("Failed to decompress chunk data")?;

	let error = match fastnbt::from_bytes(&decode_buffer) {
		Ok(value) => return Ok(value),
		Err(error) => error,
	};
	if crate::repair::repair_strings(&mut decode_buffer) == Some(true)
		&& let Ok(value) = fastnbt::from_bytes(&decode_buffer)
	{
		return Ok(value);
	}
	Err(error).context("Failed to decode NBT data")
}

/// Wraps a reader used to read a region data file
#[derive(Debug)]
pub struct Region<R: Read + Seek> {
	/// The wrapper reader
	reader: R,
	/// Where chunks too large for the region file are stored
	external: Option<External>,
}

/// Location of a region's external chunk files (`c.<x>.<z>.mcc`)
#[derive(Debug)]
struct External {
	/// Directory holding the region file
	dir: PathBuf,
	/// Region X coordinate
	x: i32,
	/// Region Z coordinate
	z: i32,
}

impl External {
	/// Derives the external chunk location from a region file path (`r.<x>.<z>.mca`)
	fn from_region_path(path: &Path) -> Option<Self> {
		let name = path.file_name()?.to_str()?;
		let mut parts = name.strip_prefix("r.")?.strip_suffix(".mca")?.split('.');
		let x = parts.next()?.parse().ok()?;
		let z = parts.next()?.parse().ok()?;
		if parts.next().is_some() {
			return None;
		}
		let dir = path.parent()?.to_path_buf();
		Some(External { dir, x, z })
	}

	/// Reads the compressed data of an external chunk
	fn read(&self, coords: ChunkCoords) -> Result<Vec<u8>> {
		let x = self.x * CHUNKS_PER_REGION as i32 + i32::from(coords.x.0);
		let z = self.z * CHUNKS_PER_REGION as i32 + i32::from(coords.z.0);
		let path = self.dir.join(format!("c.{x}.{z}.mcc"));
		std::fs::read(&path).with_context(|| format!("Failed to read {}", path.display()))
	}
}

impl<R: Read + Seek> Region<R> {
	/// Iterates over the chunks of the region data
	///
	/// The order of iteration is based on the order the chunks appear in the
	/// data file.
	pub fn foreach_chunk<T, F>(self, mut f: F) -> Result<()>
	where
		R: Read + Seek,
		T: DeserializeOwned,
		F: FnMut(ChunkCoords, T) -> Result<()>,
	{
		let Region {
			mut reader,
			external,
		} = self;

		let chunks = {
			let mut header = ChunkArray::<u32>::default();
			reader
				.read_exact(bytemuck::cast_mut::<_, [u8; BLOCKSIZE]>(&mut header.0))
				.context("Failed to read region header")?;

			parse_header(&header)
		};

		let mut seen = ChunkArray::<bool>::default();

		for ChunkDesc {
			offset,
			len,
			coords,
		} in chunks
		{
			if seen[coords] {
				bail!("Duplicate chunk {:?}", coords);
			}
			seen[coords] = true;

			reader
				.seek(SeekFrom::Start(offset as u64 * BLOCKSIZE as u64))
				.context("Failed to seek chunk data")?;

			let mut len_buf = [0u8; 4];
			reader
				.read_exact(&mut len_buf)
				.with_context(|| format!("Failed to read length for chunk {coords:?}"))?;
			let byte_len = u32::from_be_bytes(len_buf) as usize;
			if byte_len < 1 || byte_len > (len as usize) * BLOCKSIZE - 4 {
				bail!("Invalid length for chunk {:?}", coords);
			}

			let mut buffer = vec![0; byte_len];
			reader
				.read_exact(&mut buffer)
				.with_context(|| format!("Failed to read data for chunk {coords:?}"))?;
			if buffer[0] & 0x80 != 0 {
				let Some(external) = &external else {
					bail!("External data for chunk {coords:?} without a region path");
				};
				let format = buffer[0] & 0x7f;
				buffer = external.read(coords)?;
				buffer.insert(0, format);
			}
			let chunk = decode_chunk(&buffer)
				.with_context(|| format!("Failed to decode data for chunk {coords:?}"))?;

			f(coords, chunk)?;
		}

		Ok(())
	}
}

/// Creates a new [Region] from a reader
pub fn from_reader<R>(reader: R) -> Region<R>
where
	R: Read + Seek,
{
	Region {
		reader,
		external: None,
	}
}

/// Creates a new [Region] for a file
pub fn from_file<P>(path: P) -> Result<Region<File>>
where
	P: AsRef<Path>,
{
	let file = File::open(&path).context("Failed to open file")?;
	Ok(Region {
		reader: file,
		external: External::from_region_path(path.as_ref()),
	})
}

#[cfg(test)]
mod tests {
	use flate2::{Compression, write::ZlibEncoder};

	use super::*;

	/// `{"n": <value>}` as compressed NBT
	fn compressed_chunk(value: i32) -> Vec<u8> {
		let mut nbt = vec![10, 0, 0, 3, 0, 1, b'n'];
		nbt.extend(value.to_be_bytes());
		nbt.push(0);
		let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
		encoder.write_all(&nbt).unwrap();
		encoder.finish().unwrap()
	}

	/// Reads every chunk of a region file as `(x, z, n)`
	fn read_all(path: &Path) -> Result<Vec<(u8, u8, i32)>> {
		let mut chunks = vec![];
		from_file(path)?.foreach_chunk(|coords, value: fastnbt::Value| {
			let fastnbt::Value::Compound(root) = value else {
				bail!("not a compound");
			};
			let n = root["n"].as_i64().context("no n")? as i32;
			chunks.push((coords.x.0, coords.z.0, n));
			Ok(())
		})?;
		Ok(chunks)
	}

	#[test]
	fn reads_external_chunks() {
		let dir = std::env::temp_dir().join(format!("minedmap-region-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();

		// Chunk (1, 0) is stored inline, chunk (2, 3) in c.-62.99.mcc
		let mut region = vec![0u8; 2 * BLOCKSIZE];
		region[4..8].copy_from_slice(&(2 << 8 | 1u32).to_be_bytes());
		region[(2 + 3 * 32) * 4..][..4].copy_from_slice(&(3 << 8 | 1u32).to_be_bytes());
		let inline = compressed_chunk(7);
		let mut sector = ((inline.len() + 1) as u32).to_be_bytes().to_vec();
		sector.push(2);
		sector.extend(&inline);
		sector.resize(BLOCKSIZE, 0);
		region.extend(sector);
		let mut sector = 1u32.to_be_bytes().to_vec();
		sector.push(0x82);
		sector.resize(BLOCKSIZE, 0);
		region.extend(sector);

		let path = dir.join("r.-2.3.mca");
		std::fs::write(&path, &region).unwrap();
		std::fs::write(dir.join("c.-62.99.mcc"), compressed_chunk(42)).unwrap();
		let read = read_all(&path);

		std::fs::remove_file(dir.join("c.-62.99.mcc")).unwrap();
		let missing = read_all(&path);
		std::fs::remove_dir_all(&dir).unwrap();

		assert_eq!(read.unwrap(), [(1, 0, 7), (2, 3, 42)]);
		let error = format!("{:#}", missing.unwrap_err());
		assert!(error.contains("c.-62.99.mcc"), "{error}");
	}

	#[test]
	fn parses_region_names() {
		let external = External::from_region_path(Path::new("a/r.-2.13.mca")).unwrap();
		assert_eq!((external.x, external.z), (-2, 13));
		assert_eq!(external.dir, Path::new("a"));
		assert!(External::from_region_path(Path::new("r.1.mca")).is_none());
		assert!(External::from_region_path(Path::new("r.1.2.3.mca")).is_none());
	}
}
