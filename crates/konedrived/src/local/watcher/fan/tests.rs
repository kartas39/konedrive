use super::*;

fn record(kind: u8, fsid: Fsid, handle: &FileHandle, name: Option<&str>) -> Vec<u8> {
    let mut rec = vec![kind, 0, 0, 0];
    rec.extend_from_slice(&fsid[0].to_ne_bytes());
    rec.extend_from_slice(&fsid[1].to_ne_bytes());
    rec.extend_from_slice(&(handle.bytes.len() as u32).to_ne_bytes());
    rec.extend_from_slice(&handle.kind.to_ne_bytes());
    rec.extend_from_slice(&handle.bytes);
    if let Some(name) = name {
        rec.extend_from_slice(name.as_bytes());
        rec.push(0);
    }
    while rec.len() % 4 != 0 {
        rec.push(0);
    }
    let len = rec.len() as u16;
    rec[2..4].copy_from_slice(&len.to_ne_bytes());
    rec
}

fn event(mask: u64, pid: i32, records: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = records.concat();
    let mut ev = Vec::new();
    ev.extend_from_slice(&((METADATA + body.len()) as u32).to_ne_bytes());
    ev.extend_from_slice(&[3, 0]);
    ev.extend_from_slice(&(METADATA as u16).to_ne_bytes());
    ev.extend_from_slice(&mask.to_ne_bytes());
    ev.extend_from_slice(&(-1i32).to_ne_bytes());
    ev.extend_from_slice(&pid.to_ne_bytes());
    ev.extend_from_slice(&body);
    ev
}

#[test]
fn a_rename_and_an_overflow_are_read_as_the_kernel_writes_them() {
    let dir = FileHandle { kind: 1, bytes: vec![1, 2, 3, 4, 5, 6, 7, 8] };
    let obj = FileHandle { kind: 1, bytes: vec![9; 8] };
    let fsid = [7, -3];
    let mut buf = event(
        RENAME,
        42,
        &[record(INFO_OLD_DFID_NAME, fsid, &dir, Some("a")), record(INFO_NEW_DFID_NAME, fsid, &dir, Some("b")), record(INFO_FID, fsid, &obj, None)],
    );
    buf.extend(event(Q_OVERFLOW, 0, &[]));
    buf.extend(event(RENAME, 0, &[record(INFO_NEW_DFID_NAME, fsid, &dir, Some("in")), record(INFO_FID, fsid, &obj, None)]));
    let mut out = Vec::new();
    parse(&buf, &mut out);
    assert_eq!(out.len(), 3);
    let d = Fid { fsid, handle: dir };
    assert_eq!(out[0].old, Some(Named { dir: d.clone(), name: "a".into() }));
    assert_eq!(out[0].new, Some(Named { dir: d.clone(), name: "b".into() }));
    assert_eq!(out[0].object, Some(Fid { fsid, handle: obj.clone() }));
    assert_eq!(out[0].pid, 42);
    assert!(out[1].has(Q_OVERFLOW) && out[1].at.is_none() && out[1].object.is_none());
    assert_eq!((out[2].old.as_ref(), out[2].new.as_ref().map(|n| n.name.clone())), (None, Some("in".into())), "one side only");
}
