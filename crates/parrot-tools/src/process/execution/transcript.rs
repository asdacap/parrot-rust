//! Disk-backed PTY transcript with .NET-compatible UTF-16 cursors.

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use super::output::{
    MAX_OUTPUT_CHARACTERS, OutputEncoding, ProcessOutput, Utf8Decoder, invalid, temporary_file,
};

pub struct PtyTranscript {
    directory: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    file: Option<File>,
    path: Option<PathBuf>,
    decoder: Utf8Decoder,
    length: u64,
    completed: bool,
    disposed: bool,
}

impl PtyTranscript {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> io::Result<MutexGuard<'_, State>> {
        let state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("The PTY transcript lock is poisoned."))?;
        if state.disposed {
            return Err(invalid("The PTY transcript is disposed."));
        }
        Ok(state)
    }

    pub fn len(&self) -> u64 {
        self.state.lock().expect("PTY transcript lock").length
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn append(&self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.state()?;
        if state.completed {
            return Err(invalid("The PTY transcript is complete."));
        }
        self.append_locked(&mut state, bytes, false)
    }

    fn append_locked(&self, state: &mut State, bytes: &[u8], complete: bool) -> io::Result<()> {
        let units = state.decoder.append(bytes, complete);
        if units.is_empty() {
            return Ok(());
        }
        if state.file.is_none() {
            let (path, file) = temporary_file(&self.directory)?;
            state.path = Some(path);
            state.file = Some(file);
        }
        let encoded: Vec<_> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        state
            .file
            .as_mut()
            .expect("created transcript file")
            .write_all(&encoded)?;
        state.length += units.len() as u64;
        Ok(())
    }

    pub fn complete(&self) -> io::Result<()> {
        let mut state = self.state()?;
        if !state.completed {
            self.append_locked(&mut state, &[], true)?;
            state.completed = true;
            if let Some(file) = &mut state.file {
                file.flush()?;
            }
        }
        Ok(())
    }

    /// Returns raw UTF-16 units, including a low-surrogate when offset starts there.
    pub fn read(&self, offset: u64) -> io::Result<(u64, Vec<u16>)> {
        let mut state = self.state()?;
        Self::validate_offset(&state, offset)?;
        Self::read_locked(&mut state, offset)
    }

    pub fn read_text(&self, offset: u64) -> io::Result<(u64, String)> {
        let (cursor, units) = self.read(offset)?;
        Ok((cursor, String::from_utf16_lossy(&units)))
    }

    fn validate_offset(state: &State, offset: u64) -> io::Result<()> {
        if offset > state.length {
            return Err(invalid("The PTY transcript offset is out of range."));
        }
        Ok(())
    }

    fn read_locked(state: &mut State, offset: u64) -> io::Result<(u64, Vec<u16>)> {
        if offset == state.length {
            return Ok((offset, Vec::new()));
        }
        if let Some(file) = &mut state.file {
            file.flush()?;
        }
        let count = (state.length - offset).min(MAX_OUTPUT_CHARACTERS as u64) as usize;
        let path = state.path.as_ref().expect("nonempty transcript file");
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(
            offset
                .checked_mul(2)
                .ok_or_else(|| invalid("PTY offset overflow."))?,
        ))?;
        let mut bytes = vec![0; count * 2];
        file.read_exact(&mut bytes)?;
        let mut units: Vec<_> = bytes
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        if offset + (count as u64) < state.length
            && (0xd800..=0xdbff).contains(units.last().expect("nonempty read"))
        {
            let mut next = [0; 2];
            file.read_exact(&mut next)?;
            units.push(u16::from_le_bytes(next));
        }
        Ok((offset + units.len() as u64, units))
    }

    pub fn read_output(&self, offset: u64) -> io::Result<(u64, ProcessOutput)> {
        let mut state = self.state()?;
        Self::validate_offset(&state, offset)?;
        if !state.completed {
            return Err(invalid("The PTY transcript is not complete."));
        }
        let remaining = state.length - offset;
        if remaining <= MAX_OUTPUT_CHARACTERS as u64 {
            let (cursor, units) = Self::read_locked(&mut state, offset)?;
            return Ok((cursor, ProcessOutput::inline_utf16(units)));
        }
        if let Some(file) = &mut state.file {
            file.flush()?;
        }
        Ok((
            state.length,
            ProcessOutput::file_slice(
                state.path.clone().expect("nonempty transcript file"),
                OutputEncoding::Utf16LittleEndian,
                offset
                    .checked_mul(2)
                    .ok_or_else(|| invalid("PTY offset overflow."))?,
                remaining,
                false,
            ),
        ))
    }

    pub fn dispose(&self) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("The PTY transcript lock is poisoned."))?;
        if !state.disposed {
            state.disposed = true;
            state.file.take();
            if let Some(path) = state.path.take() {
                fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

impl Drop for PtyTranscript {
    fn drop(&mut self) {
        let _ = self.dispose();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::execution::output::tests::Directory;
    use std::sync::{Arc, Barrier};

    // PtyTranscriptTests.Transcript_uses_utf16_offsets_while_decoding_split_utf8
    #[test]
    fn utf16_cursors_decode_one_byte_utf8_chunks() {
        let dir = Directory::new();
        let transcript = PtyTranscript::new(&dir.0);
        let expected = "a€😀z";
        for byte in expected.as_bytes() {
            transcript.append(&[*byte]).unwrap();
        }
        transcript.complete().unwrap();
        let (cursor, units) = transcript.read(0).unwrap();
        assert_eq!(units, expected.encode_utf16().collect::<Vec<_>>());
        assert_eq!(cursor, expected.encode_utf16().count() as u64);
        assert_eq!(transcript.len(), cursor);
        assert!(transcript.read(cursor).unwrap().1.is_empty());
    }

    // PtyTranscriptTests.Returned_cursor_reads_output_appended_after_the_snapshot
    #[test]
    fn returned_cursor_reads_only_later_output() {
        let dir = Directory::new();
        let transcript = PtyTranscript::new(&dir.0);
        transcript.append("A😀".as_bytes()).unwrap();
        let (cursor, first) = transcript.read_text(0).unwrap();
        transcript.append("€B".as_bytes()).unwrap();
        let (end, second) = transcript.read_text(cursor).unwrap();
        assert_eq!(cursor, 3);
        assert_eq!(end, 5);
        assert_eq!(first + &second, "A😀€B");
    }

    // PtyTranscriptTests.Every_in_range_utf16_offset_is_accepted
    #[test]
    fn every_in_range_offset_including_low_surrogate_is_preserved() {
        let dir = Directory::new();
        let transcript = PtyTranscript::new(&dir.0);
        transcript.append("😀x".as_bytes()).unwrap();
        let (cursor, units) = transcript.read(1).unwrap();
        assert_eq!(cursor, 3);
        assert_eq!(units, vec![0xde00, b'x' as u16]);
        for offset in 0..=3 {
            assert!(transcript.read(offset).is_ok());
        }
        assert!(transcript.read(4).is_err());
        transcript.complete().unwrap();
        let (_, output) = transcript.read_output(1).unwrap();
        assert_eq!(output.text_utf16(), &[0xde00, b'x' as u16]);
    }

    // PtyTranscriptTests.Completion_flushes_an_incomplete_utf8_sequence_and_rejects_more_output
    #[test]
    fn completion_flushes_incomplete_utf8_once_and_rejects_append() {
        let dir = Directory::new();
        let transcript = PtyTranscript::new(&dir.0);
        transcript.append(&[0xe2]).unwrap();
        assert_eq!(transcript.len(), 0);
        transcript.complete().unwrap();
        transcript.complete().unwrap();
        assert_eq!(transcript.read_text(0).unwrap().1, "�");
        assert_eq!(transcript.len(), 1);
        assert!(transcript.append(b"x").is_err());
    }

    // PtyTranscriptTests.Incremental_reads_are_bounded_without_splitting_a_surrogate_pair
    #[test]
    fn bounded_reads_extend_one_unit_to_preserve_surrogate_pair() {
        let dir = Directory::new();
        let transcript = PtyTranscript::new(&dir.0);
        let prefix = "a".repeat(MAX_OUTPUT_CHARACTERS - 1);
        let expected = format!("{prefix}😀tail");
        transcript.append(expected.as_bytes()).unwrap();
        transcript.complete().unwrap();
        let (first_cursor, first) = transcript.read(0).unwrap();
        let (second_cursor, second) = transcript.read_text(first_cursor).unwrap();
        assert_eq!(first.len(), MAX_OUTPUT_CHARACTERS + 1);
        assert_eq!(String::from_utf16(&first).unwrap(), format!("{prefix}😀"));
        assert_eq!(second, "tail");
        assert_eq!(second_cursor, expected.encode_utf16().count() as u64);
        assert!(transcript.read(second_cursor).unwrap().1.is_empty());
    }

    // PtyTranscriptTests.Concurrent_reads_from_one_cursor_return_identical_snapshots
    #[test]
    fn concurrent_reads_return_identical_snapshots() {
        let dir = Directory::new();
        let transcript = Arc::new(PtyTranscript::new(&dir.0));
        transcript
            .append("界".repeat(MAX_OUTPUT_CHARACTERS + 100).as_bytes())
            .unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let transcript = Arc::clone(&transcript);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    transcript.read(0).unwrap()
                })
            })
            .collect();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert!(results.iter().all(|result| result == &results[0]));
        assert_eq!(results[0].1, vec!['界' as u16; MAX_OUTPUT_CHARACTERS]);
    }

    // PtyTranscriptTests.Spilled_completion_reads_utf16_suffix_and_cleans_up
    #[test]
    fn spilled_completion_copies_exact_utf16_suffix_and_borrowed_cleanup() {
        let dir = Directory::new();
        let transcript = PtyTranscript::new(&dir.0);
        let consumed = "a😀€";
        let remaining = "界".repeat(MAX_OUTPUT_CHARACTERS + 1) + "\u{feff}tail";
        transcript
            .append((consumed.to_owned() + &remaining).as_bytes())
            .unwrap();
        assert!(transcript.read_output(0).is_err());
        transcript.complete().unwrap();
        let (cursor, output) = transcript
            .read_output(consumed.encode_utf16().count() as u64)
            .unwrap();
        assert_eq!(
            cursor,
            (consumed.to_owned() + &remaining).encode_utf16().count() as u64
        );
        assert_eq!(output.len(), remaining.encode_utf16().count() as u64);
        assert!(output.spilled());
        let mut copied = Vec::new();
        output.copy_to(&mut copied).unwrap();
        assert_eq!(copied, remaining.as_bytes());
        let path = output.temporary_path().unwrap().to_owned();
        output.delete_temporary_file().unwrap();
        assert!(path.exists());
        transcript.dispose().unwrap();
        transcript.dispose().unwrap();
        assert!(!path.exists());
        assert!(transcript.read(0).is_err());
    }

    #[test]
    fn utf16_spill_copy_keeps_pair_across_4096_unit_copy_boundary() {
        let dir = Directory::new();
        let transcript = PtyTranscript::new(&dir.0);
        let expected = "a".repeat(4095) + "😀" + &"界".repeat(MAX_OUTPUT_CHARACTERS);
        transcript.append(expected.as_bytes()).unwrap();
        transcript.complete().unwrap();
        let (_, output) = transcript.read_output(0).unwrap();
        let mut copied = Vec::new();
        output.copy_to(&mut copied).unwrap();
        assert_eq!(copied, expected.as_bytes());
    }
}
