//! Copy into immutable-lmdb-lease/tests/ to run. Independently authored against public API.
//! Process-kill tests are NOT power-cut tests. Never dereference expired owners.
use anyhow::Result;
use immutable_lmdb_lease::Store;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

fn insert(store: &mut Store, id: u64, text: &[u8]) -> Result<(immutable_lmdb_lease::Descriptor, append_only_bytes::BytesSlice)> {
    store.insert_bound(id, text.len(), &mut |out| out.copy_from_slice(text))
}

#[test]
fn inline_and_overflow_consumers_share_binding_and_outlive_facade() -> Result<()> {
    for text in ["a玉🙂Z".as_bytes().to_vec(), "a玉🙂Z".repeat(444).into_bytes()] {
        assert!(text.len() <= 4064);
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("env");
        let mut store = unsafe { Store::open(&path)? };
        let (descriptor, owner) = insert(&mut store, 41, &text)?;
        let consumer = store.bind(&descriptor)?;
        assert_eq!(owner.as_ptr(), consumer.as_ptr());
        assert_eq!(owner.as_ref(), text.as_slice());
        let resolutions = store.metrics().resolutions;
        let second = store.bind(&descriptor)?;
        assert_eq!(second.as_ptr(), consumer.as_ptr());
        assert_eq!(store.metrics().resolutions, resolutions);
        for n in 0..32u8 {
            store.diagnostic_churn(&vec![n; 16 * 1024])?;
            assert_eq!(consumer.as_ref(), text.as_slice());
        }
        drop(owner);
        drop(second);
        drop(store);
        assert_eq!(consumer.as_ref(), text.as_slice());
        // Cooperative ownership lock must survive facade drop. Failure occurs
        // before opening a second LMDB environment in this process.
        assert!(unsafe { Store::open(&path) }.is_err());
        drop(consumer);
        let reopened = unsafe { Store::open(&path)? };
        let rebound = reopened.bind(&descriptor)?;
        assert_eq!(rebound.as_ref(), text.as_slice());
        // Do not assert equal virtual address after remapping.
    }
    Ok(())
}

#[test]
fn descriptors_reject_wrong_identity_token_length_and_missing_object() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut store = unsafe { Store::open(&temp.path().join("env"))? };
    let (good, owner) = insert(&mut store, 7, "a玉🙂Z".as_bytes())?;
    let mut bad = good.clone(); bad.store_id[0] ^= 1;
    assert!(store.bind(&bad).is_err());
    let mut bad = good.clone(); bad.page += 1;
    assert!(store.bind(&bad).is_err());
    let mut bad = good.clone(); bad.len += 1;
    assert!(store.bind(&bad).is_err()); // cached path must validate too
    assert!(store.publish(1, b"bad", &[bad.clone()]).is_err());
    assert!(store.published(1)?.is_none());
    drop(owner);
    assert!(store.bind(&bad).is_err()); // uncached path
    let mut bad = good.clone(); bad.id += 100; bad.page = bad.id;
    assert!(store.bind(&bad).is_err());
    assert!(store.publish(1, b"bad", &[bad]).is_err());
    assert!(store.published_ids()?.is_empty());
    assert!(store.insert_bound(8, 0, &mut |_| {}).is_err());
    assert!(store.insert_bound(8, 4065, &mut |x| x.fill(b'a')).is_err());
    assert!(store.insert_bound(8, 1, &mut |x| x[0] = 0xff).is_err());
    assert!(store.descriptor(8).is_err());
    let (_, retried) = insert(&mut store, 8, b"valid-after-utf8-abort")?;
    assert_eq!(retried.as_ref(), b"valid-after-utf8-abort");
    drop(retried);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = store.insert_bound(10, 32, &mut |out| {
            out[0] = b'x';
            panic!("injected generator panic before commit");
        });
    }));
    assert!(panic.is_err());
    assert!(store.descriptor(10).is_err());
    let (_, retried) = insert(&mut store, 10, b"valid-after-generator-abort")?;
    assert_eq!(retried.as_ref(), b"valid-after-generator-abort");
    assert!(insert(&mut store, 10, b"cannot-overwrite").is_err());
    Ok(())
}

fn churn(store: &mut Store, rounds: usize) -> Result<Vec<usize>> {
    let mut samples = vec![store.diagnostic_last_page_number()];
    for n in 0..rounds {
        // Value stays fixed-size and key count fixed: live logical size is constant.
        let mut bytes = vec![b'm'; 16 * 1024];
        bytes[..8].copy_from_slice(&(n as u64).to_le_bytes());
        store.diagnostic_churn(&bytes)?;
        if (n + 1) % 32 == 0 { samples.push(store.diagnostic_last_page_number()); }
    }
    Ok(samples)
}
fn growth(samples: &[usize]) -> usize { samples.last().unwrap() - samples[0] }

#[test]
fn tiny_body_lease_pins_unrelated_metadata_then_last_drop_restores_reuse() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut store = unsafe { Store::open(&temp.path().join("env"))? };
    let _ = churn(&mut store, 128)?;
    let baseline = churn(&mut store, 256)?;
    let (_, owner) = insert(&mut store, 9, b"tiny")?;
    let consumer = owner.clone();
    let held = churn(&mut store, 256)?;
    assert_eq!(owner.as_ref(), b"tiny");
    drop(owner);
    let one_held = churn(&mut store, 256)?;
    assert_eq!(consumer.as_ref(), b"tiny");
    drop(consumer);
    let settling = churn(&mut store, 64)?;
    let released = churn(&mut store, 256)?;
    eprintln!("LMDB_LIFETIME_SAMPLES baseline={baseline:?} held={held:?} one_held={one_held:?} settling={settling:?} released={released:?}");
    // Compare growth, NOT absolute file length: freeing pages is not truncation.
    assert!(growth(&held) > growth(&baseline) + 128, "held lease must expose epoch retention; inspect samples");
    assert!(growth(&one_held) > 128, "dropping one clone must not release last consumer's epoch");
    assert!(growth(&released) * 4 < growth(&one_held), "final drop must restore substantially lower allocation growth");
    Ok(())
}

// Runs as an isolated test process only when phase env is set. The parent waits
// for an explicit durable-phase marker before killing, never a guessed sleep.
#[test]
fn crash_child() -> Result<()> {
    let Ok(phase) = std::env::var("LMDB_LIFETIME_CHILD_PHASE") else { return Ok(()) };
    let path = std::path::PathBuf::from(std::env::var("LMDB_LIFETIME_PATH")?);
    let mut store = unsafe { Store::open(&path)? };
    if phase == "seed" {
        let (d, _) = insert(&mut store, 1, b"accepted-before")?;
        store.publish(1, b"accepted-before", &[d])?;
        return Ok(());
    }
    if phase.starts_with("verify-") {
        assert_eq!(store.published(1)?.as_deref(), Some(b"accepted-before".as_slice()));
        assert_eq!(store.get(1)?.as_ref(), b"accepted-before");
        if phase == "verify-private" {
            assert!(store.published(2)?.is_none());
            assert_eq!(store.published_ids()?, vec![1]);
            // Private object can survive; it must not appear as accepted state.
            assert_eq!(store.get(2)?.as_ref(), b"accepted-after");
        } else {
            assert_eq!(store.published(2)?.as_deref(), Some(b"accepted-after".as_slice()));
            assert_eq!(store.get(2)?.as_ref(), b"accepted-after");
            assert_eq!(store.published_ids()?, vec![1, 2]);
        }
        return Ok(());
    }
    let (d, _owner) = insert(&mut store, 2, b"accepted-after")?;
    if phase == "published" { store.publish(2, b"accepted-after", &[d])?; }
    assert!(phase == "private" || phase == "published");
    println!("LMDB_LIFETIME_READY:{phase}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    anyhow::bail!("child should have been killed at acknowledged phase")
}
fn child_command(path: &std::path::Path, phase: &str) -> Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command.args(["--exact", "crash_child", "--nocapture"])
        .env("LMDB_LIFETIME_CHILD_PHASE", phase).env("LMDB_LIFETIME_PATH", path);
    Ok(command)
}
#[test]
fn process_crash_distinguishes_private_orphan_from_published_state() -> Result<()> {
    for phase in ["private", "published"] {
        let temp = tempfile::tempdir()?;
        let path = temp.path().join("env");
        let seeded = child_command(&path, "seed")?.output()?;
        assert!(seeded.status.success(), "seed stderr: {}", String::from_utf8_lossy(&seeded.stderr));
        let mut child = child_command(&path, phase)?.stdout(Stdio::piped()).stdin(Stdio::piped()).stderr(Stdio::inherit()).spawn()?;
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let marker = format!("LMDB_LIFETIME_READY:{phase}");
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                match line { Ok(line) if line.trim_end().ends_with(&marker) => { let _ = send.send(()); return; }, Ok(_) => {}, Err(_) => return }
            }
        });
        let ready = receive.recv_timeout(Duration::from_secs(30));
        // Always reap even if acknowledgement failed.
        let kill_result = child.kill();
        let status = child.wait()?;
        reader.join().unwrap();
        assert!(ready.is_ok(), "missing explicit phase ack: {phase}; child={status}");
        kill_result?;
        assert!(!status.success());
        let checked = child_command(&path, &format!("verify-{phase}"))?.output()?;
        assert!(checked.status.success(), "fresh-process verifier failed: {}", String::from_utf8_lossy(&checked.stderr));
    }
    Ok(())
}
