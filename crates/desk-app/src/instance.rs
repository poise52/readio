use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::Path,
};

#[derive(Debug)]
pub struct InstanceGuard {
    _file: File,
}

impl InstanceGuard {
    pub(crate) fn acquire(root: &Path) -> io::Result<Self> {
        fs::create_dir_all(root)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".readio.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Readio уже запущен и использует этот каталог данных",
            )),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::{
        io::Read,
        ops::{Deref, DerefMut},
        path::PathBuf,
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    const ROOT_ENV: &str = "READIO_INSTANCE_TEST_ROOT";
    const RESULT_ENV: &str = "READIO_INSTANCE_TEST_RESULT";
    const HOLD_ENV: &str = "READIO_INSTANCE_TEST_HOLD";

    #[test]
    fn child() {
        let Some(root) = std::env::var_os(ROOT_ENV) else {
            return;
        };
        let result = InstanceGuard::acquire(Path::new(&root));
        let status = match &result {
            Ok(_) => "locked",
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => "busy",
            Err(error) => panic!("lock failed: {error}"),
        };
        let result_path = PathBuf::from(std::env::var_os(RESULT_ENV).unwrap());
        let temporary_path = result_path.with_extension("tmp");
        fs::write(&temporary_path, status).unwrap();
        fs::rename(temporary_path, result_path).unwrap();
        if result.is_ok() && std::env::var_os(HOLD_ENV).is_some() {
            let mut byte = [0];
            let _ = io::stdin().read(&mut byte);
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("readio-instance-{}-{nonce}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct TestChild(Child);

    impl Deref for TestChild {
        type Target = Child;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl DerefMut for TestChild {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.0
        }
    }

    impl Drop for TestChild {
        fn drop(&mut self) {
            if !matches!(self.0.try_wait(), Ok(Some(_))) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    fn spawn(root: &Path, result: &Path, hold: bool) -> TestChild {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "instance::tests::child", "--nocapture"])
            .env(ROOT_ENV, root)
            .env(RESULT_ENV, result)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if hold {
            command.env(HOLD_ENV, "1");
        } else {
            command.env_remove(HOLD_ENV);
        }
        TestChild(command.spawn().unwrap())
    }

    fn status(child: &mut Child, path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match fs::read_to_string(path) {
                Ok(status) if !status.is_empty() => return status,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => panic!("cannot read child status: {error}"),
            }
            assert!(
                child.try_wait().unwrap().is_none(),
                "child exited before reporting status"
            );
            assert!(Instant::now() < deadline, "child did not report status");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn lock_is_exclusive_across_processes_and_released_on_exit() {
        let dir = TestDir::new();
        let root = dir.0.join("data");
        let other_root = dir.0.join("other-data");
        let mut owner = spawn(&root, &dir.0.join("owner"), true);
        assert_eq!(status(&mut owner, &dir.0.join("owner")), "locked");

        let mut contender = spawn(&root, &dir.0.join("contender"), false);
        assert_eq!(status(&mut contender, &dir.0.join("contender")), "busy");
        assert!(contender.wait().unwrap().success());

        let mut independent = spawn(&other_root, &dir.0.join("independent"), false);
        assert_eq!(
            status(&mut independent, &dir.0.join("independent")),
            "locked"
        );
        assert!(independent.wait().unwrap().success());

        owner.kill().unwrap();
        owner.wait().unwrap();
        let mut successor = spawn(&root, &dir.0.join("successor"), false);
        assert_eq!(status(&mut successor, &dir.0.join("successor")), "locked");
        assert!(successor.wait().unwrap().success());
        assert!(root.join(".readio.lock").exists());
    }
}
