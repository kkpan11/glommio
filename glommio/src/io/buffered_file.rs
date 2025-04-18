// Unless explicitly stated otherwise all files in this repository are licensed
// under the MIT/Apache-2.0 License, at your convenience
//
// This product includes software developed at Datadog (https://www.datadoghq.com/). Copyright 2020 Datadog, Inc.
//

use crate::{
    io::{glommio_file::GlommioFile, read_result::ReadResult, OpenOptions},
    GlommioError,
};
use std::{
    cell::Ref,
    os::unix::io::{AsRawFd, FromRawFd, RawFd},
    path::{Path, PathBuf},
};

use super::Stat;

type Result<T> = crate::Result<T, ()>;

/// An asynchronously accessed file backed by the OS page cache.
///
/// All access uses buffered I/O, and all operations including open and close
/// are asynchronous (with some exceptions noted).
///
/// See the module-level [documentation](index.html) for more details and
/// examples.
#[derive(Debug)]
pub struct BufferedFile {
    pub(super) file: GlommioFile,
}

impl AsRawFd for BufferedFile {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl FromRawFd for BufferedFile {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        BufferedFile {
            file: GlommioFile::from_raw_fd(fd),
        }
    }
}

impl BufferedFile {
    /// Returns true if the BufferedFile represent the same file on the
    /// underlying device.
    ///
    /// Files are considered to be the same if they live in the same file system
    /// and have the same Linux inode. Note that based on this rule a
    /// symlink is *not* considered to be the same file.
    ///
    /// Files will be considered to be the same if:
    /// * A file is opened multiple times (different file descriptors, but same
    ///   file!)
    /// * they are hard links.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use glommio::{io::BufferedFile, LocalExecutor};
    /// use std::os::unix::io::AsRawFd;
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let mut wfile = BufferedFile::create("myfile.txt").await.unwrap();
    ///     let mut rfile = BufferedFile::open("myfile.txt").await.unwrap();
    ///     // Different objects (OS file descriptors), so they will be different...
    ///     assert_ne!(wfile.as_raw_fd(), rfile.as_raw_fd());
    ///     // However they represent the same object.
    ///     assert!(wfile.is_same(&rfile));
    ///     wfile.close().await;
    ///     rfile.close().await;
    /// });
    /// ```
    pub fn is_same(&self, other: &BufferedFile) -> bool {
        self.file.is_same(&other.file)
    }

    /// Similar to [`create`] in the standard library, but returns a
    /// `BufferedFile`
    ///
    /// [`create`]: https://doc.rust-lang.org/std/fs/struct.File.html#method.create
    pub async fn create<P: AsRef<Path>>(path: P) -> Result<BufferedFile> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .buffered_open(path.as_ref())
            .await
    }

    /// Similar to [`open`] in the standard library, but returns a
    /// `BufferedFile`
    ///
    /// [`open`]: https://doc.rust-lang.org/std/fs/struct.File.html#method.open
    pub async fn open<P: AsRef<Path>>(path: P) -> Result<BufferedFile> {
        OpenOptions::new()
            .read(true)
            .buffered_open(path.as_ref())
            .await
    }

    pub(super) async fn open_with_options<'a>(
        dir: RawFd,
        path: &'a Path,
        opdesc: &'static str,
        opts: &'a OpenOptions,
    ) -> Result<BufferedFile> {
        let flags = libc::O_CLOEXEC
            | opts.get_access_mode()?
            | opts.get_creation_mode()?
            | (opts.custom_flags as libc::c_int & !libc::O_ACCMODE);
        GlommioFile::open_at(dir, path, flags, opts.mode)
            .await
            .map_err(|source| GlommioError::create_enhanced(source, opdesc, Some(path), None))
            .map(|file| BufferedFile { file })
    }

    /// Write the data in the buffer `buf` to this `BufferedFile` at the
    /// specified position
    ///
    /// This method acquires ownership of the buffer so the buffer can be kept
    /// alive while the kernel has it.
    ///
    /// Note that it is legal to return fewer bytes than the buffer size. That
    /// is the situation, for example, when the device runs out of space
    /// (See the man page for `write(2)` for details)
    ///
    /// # Examples
    /// ```no_run
    /// use glommio::{io::BufferedFile, LocalExecutor};
    ///
    /// let ex = LocalExecutor::default();
    /// ex.run(async {
    ///     let file = BufferedFile::create("test.txt").await.unwrap();
    ///
    ///     let mut buf = vec![0, 1, 2, 3];
    ///     file.write_at(buf, 0).await.unwrap();
    ///     file.close().await.unwrap();
    /// });
    /// ```
    pub async fn write_at(&self, buf: Vec<u8>, pos: u64) -> Result<usize> {
        let source =
            self.file
                .reactor
                .upgrade()
                .unwrap()
                .write_buffered(self.as_raw_fd(), buf, pos);
        source.collect_rw().await.map_err(|source| {
            GlommioError::create_enhanced(
                source,
                "Writing",
                self.file.path.borrow().as_ref(),
                Some(self.as_raw_fd()),
            )
        })
    }

    /// Reads data at the specified position into a buffer allocated by this
    /// library.
    ///
    /// Note that this differs from [`DmaFile`]'s read APIs: that reflects the
    /// fact that buffered reads need no specific alignment and io_uring will
    /// not be able to use its own pre-allocated buffers for it anyway.
    ///
    /// [`DmaFile`]: struct.DmaFile.html
    /// Reads from a specific position in the file and returns the buffer.
    pub async fn read_at(&self, pos: u64, size: usize) -> Result<ReadResult> {
        let source = self.file.reactor.upgrade().unwrap().read_buffered(
            self.as_raw_fd(),
            pos,
            size,
            self.file.scheduler.borrow().as_ref(),
        );
        let read_size = source.collect_rw().await.map_err(|source| {
            GlommioError::create_enhanced(
                source,
                "Reading",
                self.file.path.borrow().as_ref(),
                Some(self.as_raw_fd()),
            )
        })?;
        Ok(ReadResult::from_sliced_buffer(source, 0, read_size))
    }

    /// Issues `fdatasync` for the underlying file, instructing the OS to flush
    /// all writes to the device, providing durability even if the system
    /// crashes or is rebooted.
    pub async fn fdatasync(&self) -> Result<()> {
        self.file.fdatasync().await
    }

    /// Erases a range from the file without changing the size. Check the man
    /// page for [`fallocate`] for a list of the supported filesystems.
    /// Partial blocks are zeroed while whole blocks are simply unmapped
    /// from the file. The reported file size (`file_size`) is unchanged but
    /// the allocated file size may if you've erased whole filesystem blocks
    /// ([`allocated_file_size`])
    ///
    /// [`fallocate`]: https://man7.org/linux/man-pages/man2/fallocate.2.html
    /// [`allocated_file_size`]: struct.Stat.html#structfield.alloc_dma_buffer
    pub async fn deallocate(&self, offset: u64, size: u64) -> Result<()> {
        self.file.deallocate(offset, size).await
    }

    /// pre-allocates space in the filesystem to hold a file at least as big as
    /// the size argument. No existing data in the range [0, size) is modified.
    /// If `keep_size` is false, then anything in [current file length, size)
    /// will report zeroed blocks until overwritten and the file size reported
    /// will be `size`. If `keep_size` is true then the existing file size
    /// is unchanged.
    pub async fn pre_allocate(&self, size: u64, keep_size: bool) -> Result<()> {
        self.file.pre_allocate(size, keep_size).await
    }

    /// Truncates a file to the specified size.
    ///
    /// Note: this syscall might be issued in a background thread depending on
    /// the system's capabilities.
    pub async fn truncate(&self, size: u64) -> Result<()> {
        self.file.truncate(size).await
    }

    /// rename this file.
    ///
    /// Note: this syscall might be issued in a background thread depending on
    /// the system's capabilities.
    pub async fn rename<P: AsRef<Path>>(&mut self, new_path: P) -> Result<()> {
        self.file.rename(new_path).await
    }

    /// remove this file.
    ///
    /// The file does not have to be closed to be removed. Removing removes
    /// the name from the filesystem but the file will still be accessible for
    /// as long as it is open.
    ///
    /// Note: this syscall might be issued in a background thread depending on
    /// the system's capabilities.
    pub async fn remove(&self) -> Result<()> {
        self.file.remove().await
    }

    /// Returns the size of a file, in bytes.
    pub async fn file_size(&self) -> Result<u64> {
        self.file.file_size().await
    }

    /// Performs a stat operation on a file to retrieve multiple properties at
    /// once.
    pub async fn stat(&self) -> Result<Stat> {
        self.file.statx().await.map(Into::into)
    }

    /// Closes this file.
    pub async fn close(self) -> Result<()> {
        self.file.close().await
    }

    /// Returns an `Option` containing the path associated with this open
    /// directory, or `None` if there isn't one.
    pub fn path(&self) -> Option<Ref<'_, Path>> {
        self.file.path()
    }

    pub(crate) fn discard(self) -> (Option<RawFd>, Option<PathBuf>) {
        self.file.discard()
    }
}

#[cfg(test)]
mod test {
    use std::convert::TryInto;

    use super::*;
    use crate::test_utils::make_test_directories;

    macro_rules! buffered_file_test {
        ( $name:ident, $dir:ident, $kind:ident, $code:block) => {
            #[test]
            fn $name() {
                for dir in make_test_directories(&format!("buffered-{}", stringify!($name))) {
                    let $dir = dir.path.clone();
                    let $kind = dir.kind;
                    test_executor!(async move { $code });
                }
            }
        };
    }

    macro_rules! check_contents {
        ( $buf:expr, $start:expr ) => {
            for (idx, i) in $buf.iter().enumerate() {
                assert_eq!(*i, ($start + (idx as u64)) as u8);
            }
        };
    }

    buffered_file_test!(file_create_close, path, _k, {
        let new_file = BufferedFile::create(path.join("testfile"))
            .await
            .expect("failed to create file");
        new_file.close().await.expect("failed to close file");
        std::assert!(path.join("testfile").exists());
    });

    buffered_file_test!(file_open, path, _k, {
        let new_file = BufferedFile::create(path.join("testfile"))
            .await
            .expect("failed to create file");
        new_file.close().await.expect("failed to close file");

        let file = BufferedFile::open(path.join("testfile"))
            .await
            .expect("failed to open file");
        file.close().await.expect("failed to close file");

        std::assert!(path.join("testfile").exists());

        let stats = crate::executor().io_stats();
        assert_eq!(stats.all_rings().files_opened(), 2);
        assert_eq!(stats.all_rings().files_closed(), 2);
    });

    buffered_file_test!(file_open_nonexistent, path, _k, {
        BufferedFile::open(path.join("testfile"))
            .await
            .expect_err("opened nonexistent file");
        std::assert!(!path.join("testfile").exists());
    });

    buffered_file_test!(random_io, path, _k, {
        let writer = BufferedFile::create(path.join("testfile")).await.unwrap();

        let reader = BufferedFile::open(path.join("testfile")).await.unwrap();

        let wb = vec![0, 1, 2, 3, 4, 5];
        let r = writer.write_at(wb, 0).await.unwrap();
        assert_eq!(r, 6);

        let rb = reader.read_at(0, 6).await.unwrap();
        assert_eq!(rb.len(), 6);
        check_contents!(*rb, 0);

        // Can read again from the same position
        let rb = reader.read_at(0, 6).await.unwrap();
        assert_eq!(rb.len(), 6);
        check_contents!(*rb, 0);

        // Can read again from a random, unaligned position, and will hit
        // EOF.
        let rb = reader.read_at(3, 6).await.unwrap();
        assert_eq!(rb.len(), 3);
        check_contents!(rb[0..3], 3);

        writer.close().await.unwrap();
        reader.close().await.unwrap();

        let stats = crate::executor().io_stats();
        assert_eq!(stats.all_rings().files_opened(), 2);
        assert_eq!(stats.all_rings().files_closed(), 2);
        assert_eq!(stats.all_rings().file_buffered_reads(), (3, 15));
        assert_eq!(stats.all_rings().file_buffered_writes(), (1, 6));
    });

    buffered_file_test!(write_past_end, path, _k, {
        let writer = BufferedFile::create(path.join("testfile")).await.unwrap();

        let reader = BufferedFile::open(path.join("testfile")).await.unwrap();

        let cluster_size = reader.stat().await.unwrap().fs_cluster_size;
        assert!(cluster_size >= 512, "{}", cluster_size);

        let rb = reader.read_at(0, 6).await.unwrap();
        assert_eq!(rb.len(), 0);

        let r = writer
            .write_at(vec![7, 8, 9, 10, 11, 12, 13], (cluster_size * 2).into())
            .await
            .unwrap();
        assert_eq!(r, 7.try_into().unwrap());

        let stat = reader.stat().await.unwrap();
        assert_eq!(stat.file_size, (cluster_size * 2 + 7).into());
        assert_eq!(stat.allocated_file_size, cluster_size.into());
        assert_eq!(stat.fs_cluster_size, cluster_size);

        let rb = reader
            .read_at(0, cluster_size.try_into().unwrap())
            .await
            .unwrap();
        assert_eq!(rb.len(), cluster_size.try_into().unwrap());
        for i in rb.iter() {
            assert_eq!(*i, 0);
        }

        let rb = reader.read_at((cluster_size * 2).into(), 40).await.unwrap();
        assert_eq!(rb.len(), 7);
        check_contents!(*rb, 7);

        let r = writer
            .write_at((513..900).map(|i| i as u8).collect(), 513)
            .await
            .unwrap();
        assert_eq!(r, 387);
        let stat = reader.stat().await.unwrap();
        // File size is unchanged.
        assert_eq!(stat.file_size, (cluster_size * 2 + 7).into());
        // Allocated size should double because the sparse region should be
        // dirtied sufficiently to need another full filesystem cluster.
        assert_eq!(stat.allocated_file_size, (cluster_size * 2).into());

        // Make sure the sparse contents are still 0'ed.
        let rb = reader.read_at(0, 513).await.unwrap();
        assert_eq!(rb.len(), 513);
        for i in rb.iter() {
            assert_eq!(*i, 0);
        }

        // Make sure the non-zeroed contents at the beginning of the file
        // are correct.
        let rb = reader.read_at(513, 387).await.unwrap();
        assert_eq!(rb.len(), 387);
        check_contents!(*rb, 513);

        // Make sure the other unallocated extent returns 0s.
        let rb = reader
            .read_at(
                900,
                ((cluster_size - 900) + cluster_size).try_into().unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            rb.len(),
            ((cluster_size - 900) + cluster_size).try_into().unwrap()
        );
        for i in rb.iter() {
            assert_eq!(*i, 0);
        }

        writer.deallocate(0, cluster_size.into()).await.unwrap();
        let stat = writer.stat().await.unwrap();
        // File size is unchanged.
        assert_eq!(stat.file_size, (cluster_size * 2 + 7).into());
        // Allocated size should go back to 0 because there's still one allocated
        // cluster.
        assert_eq!(stat.allocated_file_size, cluster_size.into());

        // Deallocated range now returns 0s.
        let rb = reader
            .read_at(0, (cluster_size).try_into().unwrap())
            .await
            .unwrap();
        assert_eq!(rb.len(), (cluster_size).try_into().unwrap());
        for i in rb.iter() {
            assert_eq!(*i, 0);
        }

        writer.close().await.unwrap();
        reader.close().await.unwrap();
    });
}
