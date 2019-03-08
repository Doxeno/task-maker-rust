use crate::execution::*;
use crate::languages::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A source file that will be able to be execute (with an optional
/// compilation step).
pub struct SourceFile {
    /// Path to the source file.
    pub path: PathBuf,
    /// Language of the source file.
    pub language: Arc<Language>,
    /// Handle to the executable after the compilation/provided file.
    pub executable: Option<File>,
}

impl SourceFile {
    /// Make a new SourceFile from the provided file. Will return None if the
    /// language is unknown.
    pub fn new(path: &Path) -> Option<SourceFile> {
        let lang = LanguageManager::detect_language(path);
        if lang.is_none() {
            return None;
        }
        Some(SourceFile {
            path: path.to_owned(),
            language: lang.unwrap(),
            executable: None,
        })
    }

    /// Execute the program relative to this source file with the specified
    /// args. If the file has not been compiled yet this may add the
    /// compilation to the dag.
    ///
    /// The returned execution has all the dependencies already set, but it has
    /// not been added to the DAG yet.
    pub fn execute(
        &mut self,
        dag: &mut ExecutionDAG,
        description: &str,
        args: Vec<String>,
    ) -> Execution {
        self.prepare(dag);
        let mut exec = Execution::new(description, self.language.runtime_command(&self.path));
        exec.args = self.language.runtime_args(&self.path, args);
        exec.input(
            self.executable.as_ref().unwrap(),
            &self.language.executable_name(&self.path),
            true,
        );
        // TODO runtime dependencies
        exec
    }

    /// Prepare the source file setting the `executable` and eventually
    /// compiling the source file.
    fn prepare(&mut self, dag: &mut ExecutionDAG) {
        if self.executable.is_some() {
            return;
        }
        if self.language.need_compilation() {
            let mut comp = Execution::new(
                &format!("Compilation of {:?}", self.path),
                self.language.compilation_command(&self.path),
            );
            comp.args = self.language.compilation_args(&self.path);
            let source = File::new(&format!("Source file of {:?}", self.path));
            comp.input(&source, Path::new(self.path.file_name().unwrap()), false);
            // TODO compilation dependencies
            let exec = comp.output(&self.language.executable_name(&self.path));
            dag.provide_file(source, &self.path);
            dag.add_execution(comp);
            // TODO bind the compilation events
            self.executable = Some(exec);
        } else {
            let executable = File::new(&format!("Source file of {:?}", self.path));
            self.executable = Some(executable.clone());
            dag.provide_file(executable, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::*;
    use crate::store::*;
    use std::io::Write;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn test_source_file_cpp() {
        env_logger::Builder::from_default_env()
            .default_format_timestamp_nanos(true)
            .init();
        let cwd = tempdir::TempDir::new("tm-test").unwrap();
        let mut dag = ExecutionDAG::new();
        let source = "int main() {return 0;}";
        let source_path = cwd.path().join("source.cpp");
        std::fs::File::create(&source_path)
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
        let mut source = SourceFile::new(&source_path).unwrap();
        let exec = source.execute(&mut dag, "Testing exec", vec![]);

        let exec_start = Arc::new(AtomicBool::new(false));
        let exec_start2 = exec_start.clone();
        let exec_done = Arc::new(AtomicBool::new(false));
        let exec_done2 = exec_done.clone();
        let exec_skipped = Arc::new(AtomicBool::new(false));
        let exec_skipped2 = exec_skipped.clone();

        dag.add_execution(exec)
            .on_start(move |_w| exec_start.store(true, Ordering::Relaxed))
            .on_done(move |_res| exec_done.store(true, Ordering::Relaxed))
            .on_skip(move || exec_skipped.store(true, Ordering::Relaxed));

        let (tx, rx_remote) = channel();
        let (tx_remote, rx) = channel();

        let server = thread::spawn(move || {
            let file_store =
                FileStore::new(Path::new("/tmp/store")).expect("Cannot create the file store");
            let mut executor = LocalExecutor::new(Arc::new(Mutex::new(file_store)), 4);
            executor.evaluate(tx_remote, rx_remote).unwrap();
        });
        ExecutorClient::evaluate(dag, tx, rx).unwrap();
        server.join().expect("Server paniced");

        assert!(exec_start2.load(Ordering::Relaxed));
        assert!(exec_done2.load(Ordering::Relaxed));
        assert!(!exec_skipped2.load(Ordering::Relaxed));
    }
}
