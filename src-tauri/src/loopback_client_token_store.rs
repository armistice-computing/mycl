use crate::app_runtime::AppHandle;
use crate::{
    loopback_client_token_types::LoopbackClientTokenStore,
    paths::loopback_client_tokens_path,
    storage::{read_json, write_secret_json},
};
use std::{path::Path, sync::Mutex};

// Atomic replacement alone does not protect read-modify-write from lost updates.
static TOKEN_STORE_TRANSACTION: Mutex<()> = Mutex::new(());

pub(crate) fn read_token_store(app: &AppHandle) -> Result<LoopbackClientTokenStore, String> {
    let path = loopback_client_tokens_path(app)?;
    read_token_store_at(&path)
}

fn read_token_store_at(path: &Path) -> Result<LoopbackClientTokenStore, String> {
    if !path.is_file() {
        return Ok(LoopbackClientTokenStore::default());
    }
    read_json::<LoopbackClientTokenStore>(path).map_err(Into::into)
}

pub(crate) fn update_token_store<T>(
    app: &AppHandle,
    update: impl FnOnce(&mut LoopbackClientTokenStore) -> Result<T, String>,
) -> Result<T, String> {
    update_token_store_at(&loopback_client_tokens_path(app)?, update)
}

fn update_token_store_at<T>(
    path: &Path,
    update: impl FnOnce(&mut LoopbackClientTokenStore) -> Result<T, String>,
) -> Result<T, String> {
    let _guard = TOKEN_STORE_TRANSACTION
        .lock()
        .map_err(|_| "loopback token store transaction lock poisoned".to_string())?;
    let mut store = read_token_store_at(path)?;
    let result = update(&mut store)?;
    write_secret_json(path, &store).map_err(String::from)?;
    Ok(result)
}

#[cfg(test)]
pub(crate) fn write_token_store(
    app: &AppHandle,
    store: &LoopbackClientTokenStore,
) -> Result<(), String> {
    write_secret_json(&loopback_client_tokens_path(app)?, store).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::loopback_client_token_types::LoopbackClientTokenRecord;
    use std::{sync::mpsc, thread, time::Duration};

    #[test]
    fn concurrent_transaction_reads_only_after_prior_revocation_is_committed() {
        let dir = std::env::temp_dir().join(format!("token-store-{}", uuid::Uuid::new_v4()));
        let path = dir.join("tokens.json");
        let record = LoopbackClientTokenRecord {
            token_id: "token".into(),
            label: "test".into(),
            token_hash: "hash".into(),
            grant_profile_id: None,
            scopes: vec!["documents.read".into()],
            home_graph_id: None,
            home_graph_set_at: None,
            created_at: "0".into(),
            expires_at: "2000".into(),
            revoked_at: None,
        };
        update_token_store_at(&path, |store| {
            store.tokens.push(record);
            Ok(())
        })
        .unwrap();

        let (loaded_tx, loaded_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let first_path = path.clone();
        let first = thread::spawn(move || {
            update_token_store_at(&first_path, |store| {
                store.tokens[0].revoked_at = Some("1000".into());
                loaded_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                Ok(())
            })
            .unwrap();
        });
        loaded_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        // The first transaction has read but has not persisted its mutation.
        assert!(matches!(
            TOKEN_STORE_TRANSACTION.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        ));
        let (started_tx, started_rx) = mpsc::channel();
        let second_path = path.clone();
        let second = thread::spawn(move || {
            started_tx.send(()).unwrap();
            update_token_store_at(&second_path, |store| {
                assert_eq!(store.tokens[0].revoked_at.as_deref(), Some("1000"));
                store.tokens[0].label = "second transaction".into();
                Ok(())
            })
            .unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        release_tx.send(()).unwrap();
        first.join().unwrap();
        second.join().unwrap();
        let persisted = read_token_store_at(&path).unwrap();
        assert_eq!(persisted.tokens[0].revoked_at.as_deref(), Some("1000"));
        assert_eq!(persisted.tokens[0].label, "second transaction");

        let failed: Result<(), String> = update_token_store_at(&path, |store| {
            store.tokens[0].revoked_at = None;
            Err("refuse mutation".into())
        });
        assert!(failed.is_err());
        assert_eq!(
            read_token_store_at(&path).unwrap().tokens[0]
                .revoked_at
                .as_deref(),
            Some("1000")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}
