//! 文件任务输出事务：独占同目录临时文件，成功后以硬链接无覆盖提交。
//! Drop 只清理本事务创建的临时文件；从不删除目标文件。
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct OutputTransaction {
    target: PathBuf,
    temporary: PathBuf,
    file: Option<File>,
}
impl OutputTransaction {
    pub fn new(target: &Path) -> io::Result<Self> {
        if target.try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "output already exists",
            ));
        }
        let parent = target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = target.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "output filename missing")
        })?;
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let mut filename = name.to_os_string();
            filename.push(format!(
                ".ye-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let temporary = parent.join(filename);
            match OpenOptions::new()
                .write(true)
                .read(true)
                .create_new(true)
                .open(&temporary)
            {
                Ok(file) => {
                    return Ok(Self {
                        target: target.to_path_buf(),
                        temporary,
                        file: Some(file),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
    }
    /// 调用方必须在 commit 前丢弃写入器，并传播 BufWriter::flush 的错误。
    pub fn writer(&self) -> io::Result<File> {
        self.file
            .as_ref()
            .ok_or_else(|| io::Error::other("output closed"))?
            .try_clone()
    }
    pub fn commit(mut self) -> io::Result<()> {
        if let Some(file) = self.file.take() {
            file.sync_all()?;
            drop(file);
        }
        // hard_link 原子创建新目录项，目标存在时失败，避免检查后被替换的竞态。
        std::fs::hard_link(&self.temporary, &self.target)?;
        Ok(())
    }
}
impl Drop for OutputTransaction {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.temporary);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn commit_collision_and_failure_preserve_existing_data() {
        let dir = std::env::temp_dir().join(format!("ye-transaction-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("result.mp4");
        let tx = OutputTransaction::new(&path).unwrap();
        tx.writer().unwrap().write_all(b"new").unwrap();
        std::fs::write(&path, b"other task").unwrap();
        assert!(tx.commit().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"other task");
        assert!(OutputTransaction::new(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        {
            let tx = OutputTransaction::new(&path).unwrap();
            tx.writer().unwrap().write_all(b"partial").unwrap();
        }
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        let tx = OutputTransaction::new(&path).unwrap();
        tx.writer().unwrap().write_all(b"complete").unwrap();
        tx.commit().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
