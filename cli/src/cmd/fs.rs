use std::collections::VecDeque;

use agentfs_sdk::PathFS;
use anyhow::Result as AnyhowResult;

use crate::cmd::init::{open_fs, resolve_db, Opened};

const ROOT_INO: i64 = 1;

pub async fn ls_filesystem(
    stdout: &mut impl std::io::Write,
    id_or_path: String,
    path: &str,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    let options = resolve_db(&id_or_path, encryption)?;
    eprintln!("Using agent: {}", id_or_path);

    let fs = open_fs(options, None).await?.arc();

    if path != "/" {
        anyhow::bail!("Only root directory (/) is currently supported");
    }

    let mut queue = VecDeque::from([(ROOT_INO, String::new())]);
    while let Some((ino, prefix)) = queue.pop_front() {
        for entry in fs.readdir_plus(ino).await?.unwrap_or_default() {
            let path = match prefix.is_empty() {
                true => entry.name,
                false => format!("{}/{}", prefix, entry.name),
            };
            let is_dir = entry.stats.is_directory();
            writeln!(stdout, "{} {}", if is_dir { 'd' } else { 'f' }, path)?;
            if is_dir {
                queue.push_back((entry.stats.ino, path));
            }
        }
    }

    Ok(())
}

pub async fn cat_filesystem(
    stdout: &mut impl std::io::Write,
    id_or_path: String,
    path: &str,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    let opened = open_fs(resolve_db(&id_or_path, encryption)?, None).await?;

    match PathFS(opened.arc()).read_file(path).await? {
        Some(file) => {
            stdout.write_all(&file)?;
            Ok(())
        }
        None => anyhow::bail!("File not found: {}", path),
    }
}

pub async fn write_filesystem(
    id_or_path: String,
    path: &str,
    content: &str,
    encryption: Option<&(String, String)>,
) -> AnyhowResult<()> {
    let opened = open_fs(resolve_db(&id_or_path, encryption)?, None).await?;
    let fs = PathFS(opened.arc());

    let names: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    let mut dir = String::new();
    for name in &names[..names.len().saturating_sub(1)] {
        dir = format!("{dir}/{name}");
        if fs.stat(&dir).await?.is_none() {
            fs.mkdir(&dir).await?;
        }
    }
    fs.write_file(path, content.as_bytes()).await?;
    Ok(())
}

pub async fn diff_filesystem(id_or_path: String) -> AnyhowResult<()> {
    let options = resolve_db(&id_or_path, None)?;
    eprintln!("Using agent: {}", id_or_path);

    match open_fs(options, None).await? {
        Opened::Merged(merged) => {
            eprintln!("Base: {}", merged.info().base_path);
            let changes = merged.diff().await?;
            if changes.is_empty() {
                println!("No changes");
            }
            for change in changes {
                println!("{change}");
            }
        }
        Opened::Plain(_) => println!("No diff (non-overlay filesystem)"),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use agentfs_sdk::{AgentFS, AgentFSOptions, EncryptionConfig};
    use tempfile::NamedTempFile;

    use crate::cmd::fs::{cat_filesystem, ls_filesystem, write_filesystem};

    const TEST_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const TEST_CIPHER: &str = "aes256gcm";

    async fn agentfs() -> (AgentFS, String, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let agentfs = AgentFS::open(AgentFSOptions::with_path(path.to_string()))
            .await
            .unwrap();
        (agentfs, file.path().to_str().unwrap().to_string(), file)
    }

    async fn encrypted_agentfs() -> (AgentFS, String, NamedTempFile) {
        let file = NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        let agentfs = AgentFS::open(AgentFSOptions::with_path(path.to_string()).with_encryption(
            EncryptionConfig {
                hex_key: TEST_KEY.to_string(),
                cipher: TEST_CIPHER.to_string(),
            },
        ))
        .await
        .unwrap();
        (agentfs, file.path().to_str().unwrap().to_string(), file)
    }

    const S_IFREG: u32 = 0o100000;

    #[tokio::test]
    pub async fn cat_file_not_found() {
        let (agentfs, path, _file) = agentfs().await;
        drop(agentfs);
        let mut buf = Vec::new();
        let err = cat_filesystem(&mut buf, path, "test.md", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("File not found"));
    }

    #[tokio::test]
    pub async fn cat_file_found() {
        let (agentfs, path, _file) = agentfs().await;
        let content = b"hello, agentfs";
        write_file(&agentfs.fs, "test.md", content, 0, 0)
            .await
            .unwrap();
        drop(agentfs);
        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path, "test.md", None)
            .await
            .unwrap();
        assert_eq!(buf, content);
    }

    #[tokio::test]
    pub async fn cat_big_file_found() {
        let (agentfs, path, _file) = agentfs().await;
        let content = vec![100u8; 4 * 1024 * 1024];
        write_file(&agentfs.fs, "test.md", &content, 0, 0)
            .await
            .unwrap();
        drop(agentfs);
        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path, "test.md", None)
            .await
            .unwrap();
        assert_eq!(buf, content);
    }

    #[tokio::test]
    pub async fn ls_empty() {
        let (agentfs, path, _file) = agentfs().await;
        drop(agentfs);
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path, "/", None).await.unwrap();
        assert_eq!(buf, b"");
    }

    #[tokio::test]
    pub async fn ls_files_only() {
        let (agentfs, path, _file) = agentfs().await;
        write_file(&agentfs.fs, "1.md", b"1", 0, 0).await.unwrap();
        write_file(&agentfs.fs, "2.md", b"11", 0, 0).await.unwrap();
        let big = vec![100u8; 1024 * 1024];
        write_file(&agentfs.fs, "3.md", &big, 0, 0).await.unwrap();
        drop(agentfs);
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path, "/", None).await.unwrap();
        assert_eq!(
            buf,
            b"f 1.md
f 2.md
f 3.md
"
        );
    }

    #[tokio::test]
    pub async fn ls_dirs() {
        let (agentfs, path, _file) = agentfs().await;
        agentfs.fs.mkdir("a", 0, 0).await.unwrap();
        agentfs.fs.mkdir("a/b", 0, 0).await.unwrap();
        agentfs.fs.mkdir("a/c", 0, 0).await.unwrap();
        agentfs.fs.mkdir("d", 0, 0).await.unwrap();
        agentfs.fs.mkdir("d/e", 0, 0).await.unwrap();
        write_file(&agentfs.fs, "a/b/1.md", b"1", 0, 0)
            .await
            .unwrap();
        write_file(&agentfs.fs, "a/c/2.md", b"11", 0, 0)
            .await
            .unwrap();
        let big = vec![100u8; 1024 * 1024];
        write_file(&agentfs.fs, "d/e/3.md", &big, 0, 0)
            .await
            .unwrap();
        drop(agentfs);
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path, "/", None).await.unwrap();
        assert_eq!(
            buf,
            b"d a
d d
d a/b
d a/c
d d/e
f a/b/1.md
f a/c/2.md
f d/e/3.md
"
        );
    }

    // Encryption tests

    #[tokio::test]
    pub async fn encrypted_write_and_cat() {
        let (agentfs, path, _file) = encrypted_agentfs().await;
        let content = b"encrypted content";
        write_file(&agentfs.fs, "secret.txt", content, 0, 0)
            .await
            .unwrap();
        drop(agentfs);

        let encryption = Some((TEST_KEY.to_string(), TEST_CIPHER.to_string()));
        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path, "secret.txt", encryption.as_ref())
            .await
            .unwrap();
        assert_eq!(buf, content);
    }

    #[tokio::test]
    pub async fn encrypted_ls() {
        let (agentfs, path, _file) = encrypted_agentfs().await;
        write_file(&agentfs.fs, "file1.txt", b"1", 0, 0)
            .await
            .unwrap();
        write_file(&agentfs.fs, "file2.txt", b"2", 0, 0)
            .await
            .unwrap();
        drop(agentfs);

        let encryption = Some((TEST_KEY.to_string(), TEST_CIPHER.to_string()));
        let mut buf = Vec::new();
        ls_filesystem(&mut buf, path, "/", encryption.as_ref())
            .await
            .unwrap();
        assert_eq!(buf, b"f file1.txt\nf file2.txt\n");
    }

    #[tokio::test]
    pub async fn encrypted_write_filesystem() {
        let (agentfs, path, _file) = encrypted_agentfs().await;
        drop(agentfs);

        let encryption = Some((TEST_KEY.to_string(), TEST_CIPHER.to_string()));
        write_filesystem(
            path.clone(),
            "/new_file.txt",
            "new content",
            encryption.as_ref(),
        )
        .await
        .unwrap();

        let mut buf = Vec::new();
        cat_filesystem(&mut buf, path, "/new_file.txt", encryption.as_ref())
            .await
            .unwrap();
        assert_eq!(buf, b"new content");
    }

    async fn write_file(
        fs: &agentfs_sdk::filesystem::AgentFS,
        path: &str,
        data: &[u8],
        uid: u32,
        gid: u32,
    ) -> anyhow::Result<()> {
        if fs.stat(path).await?.is_some() {
            fs.remove(path).await?;
        }
        let (_, file) = fs.create_file(path, S_IFREG | 0o644, uid, gid).await?;
        file.pwrite(0, data).await?;
        Ok(())
    }
}
