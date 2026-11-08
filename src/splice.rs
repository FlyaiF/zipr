//! Streaming ZIP rewrite with verbatim preservation of untouched records.
use crate::engine::{CancelToken, EngineError};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
};
type Result<T> = std::result::Result<T, EngineError>;
pub(crate) fn u16le(b: &[u8], p: usize) -> u16 {
    u16::from_le_bytes([b[p], b[p + 1]])
}
pub(crate) fn u32le(b: &[u8], p: usize) -> u32 {
    u32::from_le_bytes(b[p..p + 4].try_into().expect("fixed width"))
}
fn u64le(b: &[u8], p: usize) -> u64 {
    u64::from_le_bytes(b[p..p + 8].try_into().expect("fixed width"))
}
fn put16(b: &mut [u8], p: usize, n: u16) {
    b[p..p + 2].copy_from_slice(&n.to_le_bytes());
}
fn put32(b: &mut [u8], p: usize, n: u32) {
    b[p..p + 4].copy_from_slice(&n.to_le_bytes());
}
fn invalid(s: &str) -> EngineError {
    EngineError::Invalid(s.into())
}
#[derive(Clone, Debug)]
pub struct Record {
    pub raw: Vec<u8>,
    pub offset: u64,
    pub size: u64,
    pub compressed: u64,
    pub name: String,
    pub raw_name: Vec<u8>,
}
impl Record {
    pub fn method(&self) -> u16 {
        u16le(&self.raw, 10)
    }
    pub fn crc(&self) -> u32 {
        u32le(&self.raw, 16)
    }
    pub fn data_offset<R: Read + Seek>(&self, r: &mut R) -> Result<u64> {
        r.seek(SeekFrom::Start(self.offset))?;
        let mut h = [0; 30];
        r.read_exact(&mut h)?;
        if u32le(&h, 0) != 0x04034b50 {
            return Err(invalid("bad local header"));
        }
        Ok(self.offset + 30 + u16le(&h, 26) as u64 + u16le(&h, 28) as u64)
    }
}
pub struct Directory {
    pub records: Vec<Record>,
    pub start: u64,
    tail: Vec<u8>,
    zip64: Option<usize>,
}
impl Directory {
    pub fn read<R: Read + Seek>(r: &mut R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        let n = len.min(65557) as usize;
        r.seek(SeekFrom::End(-(n as i64)))?;
        let mut end = vec![0; n];
        r.read_exact(&mut end)?;
        let p = (0..n.saturating_sub(21))
            .rev()
            .find(|&p| u32le(&end, p) == 0x06054b50 && p + 22 + u16le(&end, p + 20) as usize == n)
            .ok_or_else(|| invalid("missing EOCD"))?;
        if u16le(&end, p + 4) != 0 || u16le(&end, p + 6) != 0 {
            return Err(invalid("multi-disk ZIP is unsupported"));
        }
        let ep = len - n as u64 + p as u64;
        let mut count = u16le(&end, p + 10) as u64;
        let mut start = u32le(&end, p + 16) as u64;
        let mut cd_size = u32le(&end, p + 12) as u64;
        let mut tail_start = ep;
        let mut z = None;
        if ep >= 20 {
            r.seek(SeekFrom::Start(ep - 20))?;
            let mut loc = [0; 20];
            r.read_exact(&mut loc)?;
            if u32le(&loc, 0) == 0x07064b50 {
                tail_start = u64le(&loc, 8);
                let mut h = [0; 56];
                let valid = if tail_start.checked_add(56).is_some_and(|v| v <= ep - 20) {
                    r.seek(SeekFrom::Start(tail_start))?;
                    r.read_exact(&mut h)?;
                    u32le(&h, 0) == 0x06064b50
                        && tail_start
                            .checked_add(12)
                            .and_then(|v| v.checked_add(u64le(&h, 4)))
                            == Some(ep - 20)
                } else {
                    false
                };
                if !valid {
                    // A launch script may have been prepended without adjusting
                    // the ZIP-relative locator. Search backwards in bounded blocks.
                    let mut end = ep - 20;
                    'search: loop {
                        let begin = end.saturating_sub(65536);
                        r.seek(SeekFrom::Start(begin))?;
                        let mut block = vec![0; (end - begin) as usize];
                        r.read_exact(&mut block)?;
                        for q in (0..block.len().saturating_sub(11)).rev() {
                            if u32le(&block, q) == 0x06064b50
                                && (begin + q as u64)
                                    .checked_add(12)
                                    .and_then(|v| v.checked_add(u64le(&block, q + 4)))
                                    == Some(ep - 20)
                            {
                                tail_start = begin + q as u64;
                                r.seek(SeekFrom::Start(tail_start))?;
                                r.read_exact(&mut h)?;
                                break 'search;
                            }
                        }
                        if begin == 0 {
                            return Err(invalid("bad ZIP64"));
                        }
                        end = begin + 11;
                    }
                }
                count = u64le(&h, 32);
                start = u64le(&h, 48);
                cd_size = u64le(&h, 40);
                z = Some(0);
            }
        }
        if count > len / 46 || start > tail_start {
            return Err(invalid("invalid directory bounds"));
        }
        let base = tail_start
            .checked_sub(cd_size)
            .and_then(|v| v.checked_sub(start))
            .ok_or_else(|| invalid("directory size"))?;
        start += base;
        r.seek(SeekFrom::Start(start))?;
        let mut records = Vec::new();
        for _ in 0..count {
            let mut raw = vec![0; 46];
            r.read_exact(&mut raw)?;
            if u32le(&raw, 0) != 0x02014b50 {
                return Err(invalid("bad central record"));
            }
            let nl = u16le(&raw, 28) as usize;
            let el = u16le(&raw, 30) as usize;
            let cl = u16le(&raw, 32) as usize;
            raw.resize(46 + nl + el + cl, 0);
            r.read_exact(&mut raw[46..])?;
            let mut size = u32le(&raw, 24) as u64;
            let mut compressed = u32le(&raw, 20) as u64;
            let mut offset = u32le(&raw, 42) as u64;
            let mut x = 46 + nl;
            while x + 4 <= 46 + nl + el {
                let id = u16le(&raw, x);
                let l = u16le(&raw, x + 2) as usize;
                if x + 4 + l > 46 + nl + el {
                    return Err(invalid("bad extra field"));
                }
                if id == 1 {
                    let mut q = x + 4;
                    for v in [&mut size, &mut compressed, &mut offset] {
                        if *v == u32::MAX as u64 {
                            if q + 8 > x + 4 + l {
                                return Err(invalid("short ZIP64 field"));
                            }
                            *v = u64le(&raw, q);
                            q += 8;
                        }
                    }
                }
                x += 4 + l;
            }
            let raw_name = raw[46..46 + nl].to_vec();
            let name = match std::str::from_utf8(&raw_name) {
                Ok(s) => s.to_owned(),
                Err(_) if u16le(&raw, 8) & 0x800 != 0 => {
                    String::from_utf8_lossy(&raw_name).into_owned()
                }
                Err(_) => encoding_rs::GBK.decode(&raw_name).0.into_owned(),
            };
            let offset = offset
                .checked_add(base)
                .filter(|o| *o < start)
                .ok_or_else(|| invalid("local offset outside archive data"))?;
            records.push(Record {
                raw,
                offset,
                size,
                compressed,
                name,
                raw_name,
            });
        }
        let pos = r.stream_position()?;
        if pos > tail_start {
            return Err(invalid("directory overlaps trailer"));
        }
        r.seek(SeekFrom::Start(pos))?;
        let mut tail = vec![0; (len - pos) as usize];
        r.read_exact(&mut tail)?;
        if z.is_some() {
            z = Some((tail_start - pos) as usize);
        }
        Ok(Self {
            records,
            start,
            tail,
            zip64: z,
        })
    }
}
#[derive(Clone)]
pub struct Change {
    pub source: PathBuf,
}
pub(crate) fn copy<R: Read, W: Write>(r: &mut R, w: &mut W, c: &CancelToken) -> Result<u64> {
    let mut b = [0; 128 * 1024];
    let mut n = 0;
    loop {
        c.check()?;
        let k = r.read(&mut b)?;
        if k == 0 {
            break;
        }
        w.write_all(&b[..k])?;
        n += k as u64;
    }
    Ok(n)
}
fn offset_record(raw: &mut Vec<u8>, offset: u64) -> Result<()> {
    if u32le(raw, 42) != u32::MAX {
        if offset >= u32::MAX as u64 {
            let nl = u16le(raw, 28) as usize;
            let el = u16le(raw, 30) as usize;
            let mut extra = raw[46 + nl..46 + nl + el].to_vec();
            let mut values = Vec::new();
            // Existing size slots precede the newly promoted offset slot.
            if u32le(raw, 24) == u32::MAX || u32le(raw, 20) == u32::MAX {
                let old = zip64_payload(&extra)?;
                let count = usize::from(u32le(raw, 24) == u32::MAX)
                    + usize::from(u32le(raw, 20) == u32::MAX);
                values.extend_from_slice(
                    old.get(..count * 8)
                        .ok_or_else(|| invalid("short ZIP64 sizes"))?,
                );
            }
            values.extend_from_slice(&offset.to_le_bytes());
            set_zip64(&mut extra, &values)?;
            raw.splice(46 + nl..46 + nl + el, extra.iter().copied());
            put16(raw, 30, extra.len() as u16);
            put32(raw, 42, u32::MAX);
            return Ok(());
        }
        put32(raw, 42, offset as u32);
        return Ok(());
    }
    let mut x = 46 + u16le(raw, 28) as usize;
    let stop = x + u16le(raw, 30) as usize;
    while x + 4 <= stop {
        let l = u16le(raw, x + 2) as usize;
        if u16le(raw, x) == 1 {
            let q = x
                + 4
                + if u32le(raw, 24) == u32::MAX { 8 } else { 0 }
                + if u32le(raw, 20) == u32::MAX { 8 } else { 0 };
            if q + 8 > x + 4 + l {
                return Err(invalid("missing ZIP64 offset"));
            }
            raw[q..q + 8].copy_from_slice(&offset.to_le_bytes());
            return Ok(());
        }
        x += 4 + l;
    }
    Err(invalid("missing ZIP64 offset"))
}
/// Changes are keyed by decoded entry name. Untouched local spans and metadata are copied verbatim.
pub fn rewrite<R: Read + Seek, W: Write>(
    r: &mut R,
    w: &mut W,
    changes: &BTreeMap<String, Change>,
    cancel: &CancelToken,
) -> Result<()> {
    let d = Directory::read(r)?;
    let mut out = 0u64;
    let first = d.records.iter().map(|r| r.offset).min().unwrap_or(d.start);
    r.seek(SeekFrom::Start(0))?;
    out += copy(&mut r.take(first), w, cancel)?;
    let mut offsets: Vec<u64> = d.records.iter().map(|r| r.offset).collect();
    offsets.sort_unstable();
    offsets.push(d.start);
    let mut central = Vec::new();
    for rec in &d.records {
        cancel.check()?;
        let at = out;
        let mut raw = rec.raw.clone();
        if let Some(change) = changes.get(&rec.name) {
            out += replacement(r, w, rec, change, &mut raw, cancel)?;
        } else {
            let i = offsets
                .binary_search(&rec.offset)
                .map_err(|_| invalid("offset"))?;
            let len = offsets[i + 1] - rec.offset;
            r.seek(SeekFrom::Start(rec.offset))?;
            out += copy(&mut r.take(len), w, cancel)?;
        }
        offset_record(&mut raw, at)?;
        central.push(raw);
    }
    for (name, change) in changes {
        if d.records.iter().any(|r| r.name == *name) {
            continue;
        }
        let bytes = name.as_bytes();
        if bytes.len() > 65535 {
            return Err(invalid("name too long"));
        }
        let mut raw = vec![0; 46];
        put32(&mut raw, 0, 0x02014b50);
        put16(&mut raw, 4, 20);
        put16(&mut raw, 6, 20);
        put16(&mut raw, 8, 0x800);
        put16(
            &mut raw,
            10,
            if [".jar", ".zip", ".war"].iter().any(|x| name.ends_with(x)) {
                0
            } else {
                8
            },
        );
        put16(&mut raw, 28, bytes.len() as u16);
        raw.extend_from_slice(bytes);
        let rec = Record {
            raw: raw.clone(),
            offset: 0,
            size: 0,
            compressed: 0,
            name: name.clone(),
            raw_name: bytes.to_vec(),
        };
        let at = out;
        out += replacement_new(w, &rec, change, &mut raw, cancel)?;
        offset_record(&mut raw, at)?;
        central.push(raw);
    }
    let start = out;
    for raw in &central {
        w.write_all(raw)?;
        out += raw.len() as u64;
    }
    let size = out - start;
    let mut tail = d.tail;
    let ep = (0..tail.len().saturating_sub(21))
        .rev()
        .find(|&p| {
            u32le(&tail, p) == 0x06054b50 && p + 22 + u16le(&tail, p + 20) as usize == tail.len()
        })
        .ok_or_else(|| invalid("EOCD"))?;
    if let Some(z) = d.zip64 {
        tail[z + 24..z + 32].copy_from_slice(&(central.len() as u64).to_le_bytes());
        tail[z + 32..z + 40].copy_from_slice(&(central.len() as u64).to_le_bytes());
        tail[z + 40..z + 48].copy_from_slice(&size.to_le_bytes());
        tail[z + 48..z + 56].copy_from_slice(&start.to_le_bytes());
        tail[ep - 12..ep - 4].copy_from_slice(&(out + z as u64).to_le_bytes());
    } else if central.len() >= 65535 || start >= u32::MAX as u64 || size >= u32::MAX as u64 {
        let mut z = vec![0; 76];
        put32(&mut z, 0, 0x06064b50);
        z[4..12].copy_from_slice(&44u64.to_le_bytes());
        put16(&mut z, 12, 45);
        put16(&mut z, 14, 45);
        z[24..32].copy_from_slice(&(central.len() as u64).to_le_bytes());
        z[32..40].copy_from_slice(&(central.len() as u64).to_le_bytes());
        z[40..48].copy_from_slice(&size.to_le_bytes());
        z[48..56].copy_from_slice(&start.to_le_bytes());
        put32(&mut z, 56, 0x07064b50);
        z[64..72].copy_from_slice(&out.to_le_bytes());
        put32(&mut z, 72, 1);
        w.write_all(&z)?;
    }
    put16(&mut tail, ep + 8, central.len().min(65535) as u16);
    put16(&mut tail, ep + 10, central.len().min(65535) as u16);
    if u32le(&tail, ep + 12) != u32::MAX {
        put32(&mut tail, ep + 12, size.min(u32::MAX as u64) as u32);
    }
    if u32le(&tail, ep + 16) != u32::MAX {
        put32(&mut tail, ep + 16, start.min(u32::MAX as u64) as u32);
    }
    w.write_all(&tail)?;
    Ok(())
}
fn replacement<R: Read + Seek, W: Write>(
    r: &mut R,
    w: &mut W,
    rec: &Record,
    ch: &Change,
    raw: &mut Vec<u8>,
    c: &CancelToken,
) -> Result<u64> {
    r.seek(SeekFrom::Start(rec.offset))?;
    let mut h = [0; 30];
    r.read_exact(&mut h)?;
    let mut local = vec![0; u16le(&h, 26) as usize + u16le(&h, 28) as usize];
    r.read_exact(&mut local)?;
    write_replacement(w, rec, ch, raw, h, local, c)
}
fn replacement_new<W: Write>(
    w: &mut W,
    rec: &Record,
    ch: &Change,
    raw: &mut Vec<u8>,
    c: &CancelToken,
) -> Result<u64> {
    let mut h = [0; 30];
    put32(&mut h, 0, 0x04034b50);
    h[4..10].copy_from_slice(&raw[6..12]);
    put16(&mut h, 26, rec.raw_name.len() as u16);
    write_replacement(w, rec, ch, raw, h, rec.raw_name.clone(), c)
}
fn write_replacement<W: Write>(
    w: &mut W,
    rec: &Record,
    ch: &Change,
    raw: &mut Vec<u8>,
    mut h: [u8; 30],
    mut local: Vec<u8>,
    c: &CancelToken,
) -> Result<u64> {
    if u16le(raw, 8) & 1 != 0 {
        return Err(invalid("encrypted entry"));
    }
    let mut src = File::open(&ch.source)?;
    let mut tmp = tempfile::tempfile()?;
    let mut hash = crc32fast::Hasher::new();
    let mut size = 0u64;
    let mut b = [0; 128 * 1024];
    if rec.method() == 8 && src.metadata()?.len() > 8 * 1024 * 1024 {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        loop {
            let mut chunks = Vec::new();
            for _ in 0..cores {
                c.check()?;
                let mut chunk = Vec::new();
                Read::by_ref(&mut src)
                    .take(1024 * 1024)
                    .read_to_end(&mut chunk)?;
                if chunk.is_empty() {
                    break;
                }
                hash.update(&chunk);
                size += chunk.len() as u64;
                chunks.push(chunk);
            }
            if chunks.is_empty() {
                break;
            }
            let compressed = std::thread::scope(|scope| {
                let jobs: Vec<_> = chunks
                    .into_iter()
                    .map(|chunk| {
                        scope.spawn(move || {
                            let mut compressor =
                                flate2::Compress::new(flate2::Compression::new(6), false);
                            let mut out = Vec::with_capacity(chunk.len() * 2 + 1024);
                            compressor
                                .compress_vec(&chunk, &mut out, flate2::FlushCompress::Sync)
                                .map_err(|e| invalid(&e.to_string()))?;
                            if compressor.total_in() != chunk.len() as u64 {
                                return Err(invalid("short deflate chunk"));
                            }
                            Ok(out)
                        })
                    })
                    .collect();
                jobs.into_iter()
                    .map(|j| j.join().map_err(|_| invalid("deflate worker failed"))?)
                    .collect::<Result<Vec<_>>>()
            })?;
            for chunk in compressed {
                c.check()?;
                tmp.write_all(&chunk)?;
            }
        }
        tmp.write_all(&[3, 0])?;
    } else {
        fn stream(
            src: &mut File,
            dst: &mut dyn Write,
            hash: &mut crc32fast::Hasher,
            size: &mut u64,
            b: &mut [u8],
            c: &CancelToken,
        ) -> Result<()> {
            loop {
                c.check()?;
                let n = src.read(b)?;
                if n == 0 {
                    break;
                }
                hash.update(&b[..n]);
                *size += n as u64;
                dst.write_all(&b[..n])?;
            }
            Ok(())
        }
        match rec.method() {
            0 => stream(&mut src, &mut tmp, &mut hash, &mut size, &mut b, c)?,
            8 => {
                let mut encoder =
                    flate2::write::DeflateEncoder::new(&mut tmp, flate2::Compression::new(6));
                stream(&mut src, &mut encoder, &mut hash, &mut size, &mut b, c)?;
                encoder.finish()?;
            }
            _ => return Err(invalid("unsupported compression")),
        }
    }
    let compressed = tmp.metadata()?.len();
    let crc = hash.finalize();
    let flags = u16le(raw, 8) & !8;
    put16(raw, 8, flags);
    put16(&mut h, 6, flags);
    let size64 = size >= u32::MAX as u64 || u32le(raw, 24) == u32::MAX;
    let compressed64 = compressed >= u32::MAX as u64 || u32le(raw, 20) == u32::MAX;
    let offset64 = u32le(raw, 42) == u32::MAX;
    if size64 || compressed64 || offset64 {
        let nl = u16le(raw, 28) as usize;
        let el = u16le(raw, 30) as usize;
        let mut extra = raw[46 + nl..46 + nl + el].to_vec();
        let mut values = Vec::new();
        if size64 {
            values.extend_from_slice(&size.to_le_bytes());
        }
        if compressed64 {
            values.extend_from_slice(&compressed.to_le_bytes());
        }
        if offset64 {
            values.extend_from_slice(&rec.offset.to_le_bytes());
        }
        set_zip64(&mut extra, &values)?;
        raw.splice(46 + nl..46 + nl + el, extra.iter().copied());
        put16(raw, 30, extra.len() as u16);
    }
    put32(raw, 16, crc);
    put32(
        raw,
        20,
        if compressed64 {
            u32::MAX
        } else {
            compressed as u32
        },
    );
    put32(raw, 24, if size64 { u32::MAX } else { size as u32 });
    let local64 = size >= u32::MAX as u64
        || compressed >= u32::MAX as u64
        || u32le(&h, 18) == u32::MAX
        || u32le(&h, 22) == u32::MAX;
    if local64 {
        let mut extra = local[rec.raw_name.len()..].to_vec();
        let mut values = size.to_le_bytes().to_vec();
        values.extend_from_slice(&compressed.to_le_bytes());
        set_zip64(&mut extra, &values)?;
        local.truncate(rec.raw_name.len());
        local.extend_from_slice(&extra);
        put16(&mut h, 28, extra.len() as u16);
    }
    put32(&mut h, 14, crc);
    put32(
        &mut h,
        18,
        if local64 { u32::MAX } else { compressed as u32 },
    );
    put32(&mut h, 22, if local64 { u32::MAX } else { size as u32 });
    let modified = src.metadata()?.modified()?;
    let dt = time::OffsetDateTime::from(modified);
    let dt = dt.to_offset(time::UtcOffset::local_offset_at(dt).unwrap_or(time::UtcOffset::UTC));
    let year = dt.year().clamp(1980, 2107) as u16;
    let date = ((year - 1980) << 9) | ((dt.month() as u16) << 5) | dt.day() as u16;
    let time = ((dt.hour() as u16) << 11) | ((dt.minute() as u16) << 5) | (dt.second() as u16 / 2);
    put16(raw, 12, time);
    put16(raw, 14, date);
    put16(&mut h, 10, time);
    put16(&mut h, 12, date);
    fn timestamp(extra: &mut [u8], unix: u32) {
        let mut p = 0;
        while p + 4 <= extra.len() {
            let n = u16le(extra, p + 2) as usize;
            if p + 4 + n > extra.len() {
                break;
            }
            if u16le(extra, p) == 0x5455 && n >= 5 && extra[p + 4] & 1 != 0 {
                put32(extra, p + 5, unix);
            }
            p += 4 + n;
        }
    }
    let nl = u16le(raw, 28) as usize;
    let el = u16le(raw, 30) as usize;
    timestamp(&mut raw[46 + nl..46 + nl + el], dt.unix_timestamp() as u32);
    timestamp(&mut local[rec.raw_name.len()..], dt.unix_timestamp() as u32);
    w.write_all(&h)?;
    w.write_all(&local)?;
    tmp.rewind()?;
    copy(&mut tmp, w, c)?;
    Ok(30 + local.len() as u64 + compressed)
}

fn zip64_payload(extra: &[u8]) -> Result<&[u8]> {
    let mut p = 0;
    while p + 4 <= extra.len() {
        let n = u16le(extra, p + 2) as usize;
        if p + 4 + n > extra.len() {
            return Err(invalid("extra bounds"));
        }
        if u16le(extra, p) == 1 {
            return Ok(&extra[p + 4..p + 4 + n]);
        }
        p += 4 + n;
    }
    Ok(&[])
}
fn set_zip64(extra: &mut Vec<u8>, values: &[u8]) -> Result<()> {
    let mut p = 0;
    while p + 4 <= extra.len() {
        let n = u16le(extra, p + 2) as usize;
        if p + 4 + n > extra.len() {
            return Err(invalid("extra bounds"));
        }
        if u16le(extra, p) == 1 {
            extra.splice(p + 4..p + 4 + n, values.iter().copied());
            put16(extra, p + 2, values.len() as u16);
            return Ok(());
        }
        p += 4 + n;
    }
    if extra.len() + 4 + values.len() > 65535 {
        return Err(invalid("extra field too large"));
    }
    extra.extend_from_slice(&1u16.to_le_bytes());
    extra.extend_from_slice(&(values.len() as u16).to_le_bytes());
    extra.extend_from_slice(values);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn promotes_and_updates_zip64_offsets() {
        let mut raw = vec![0; 46];
        put32(&mut raw, 0, 0x02014b50);
        offset_record(&mut raw, u32::MAX as u64 + 100).unwrap();
        assert_eq!(u32le(&raw, 42), u32::MAX);
        assert_eq!(
            u64le(zip64_payload(&raw[46..]).unwrap(), 0),
            u32::MAX as u64 + 100
        );
        offset_record(&mut raw, 123).unwrap();
        assert_eq!(u64le(zip64_payload(&raw[46..]).unwrap(), 0), 123);
    }
}
