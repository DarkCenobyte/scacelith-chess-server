//! The owner file of a shard's directory: the listing of the shard directories, the lock, the
//! recorded server id, and a journal that ignores the file.

use std::fs;

use super::support::{TempDir, opts};
use crate::journal::owner::{ClaimError, ShardClaim, shard_dirs};
use crate::journal::{Journal, RecordKind};

#[test]
fn shard_directories_are_listed_in_order_and_other_names_ignored() {
    let dir = TempDir::new("owner-list");
    assert_eq!(shard_dirs(&dir.path().join("missing")).expect("listed"), Vec::<u32>::new());
    for name in ["shard-12", "shard-0", "shard-3", "shard-63", "shard-64", "shard-07", "shard-x", "other"] {
        fs::create_dir_all(dir.path().join(name)).expect("directory");
    }
    fs::write(dir.path().join("shard-5"), b"a file").expect("file");
    assert_eq!(shard_dirs(dir.path()).expect("listed"), [0, 3, 12, 63]);
}

#[test]
fn a_claim_locks_the_shard_and_keeps_its_owner() {
    let dir = TempDir::new("owner-claim");
    let mut claim = ShardClaim::take(dir.path(), 2).expect("claimed");
    assert_eq!((claim.shard(), claim.owner(), claim.locked()), (2, None, true));
    assert!(matches!(ShardClaim::take(dir.path(), 2), Err(ClaimError::InUse)), "one process at a time");
    claim.set_owner("abc").expect("written");
    claim.set_owner("abc").expect("unchanged");
    assert_eq!(fs::read_to_string(claim.path()).expect("read"), "abc\n");
    drop(claim);
    let mut claim = ShardClaim::take(dir.path(), 2).expect("free again");
    assert_eq!(claim.owner(), Some("abc"));
    claim.set_owner("de").expect("written");
    assert_eq!(fs::read_to_string(claim.path()).expect("read"), "de\n");
    // Another shard is another lock.
    let other = ShardClaim::take(dir.path(), 3).expect("claimed");
    assert_eq!(other.owner(), None);
}

#[tokio::test]
async fn a_journal_ignores_the_owner_file_of_its_directory() {
    let dir = TempDir::new("owner-journal");
    let j = Journal::open(opts(dir.path())).await.expect("journal");
    j.append(RecordKind::Created, 7, b"x", 1.0).expect("appended");
    j.close().await.expect("closed");
    let mut claim = ShardClaim::take(dir.path(), 0).expect("claimed");
    claim.set_owner("0123456789abcdef").expect("written");
    let j = Journal::open(opts(dir.path())).await.expect("journal");
    assert_eq!(j.recover().keys().copied().collect::<Vec<_>>(), [7]);
    assert!(j.stats().recovery.problems.is_empty());
    j.close().await.expect("closed");
    assert!(claim.path().exists(), "the journal leaves the file alone");
}
