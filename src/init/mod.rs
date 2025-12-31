use std::path::PathBuf;
mod server;
pub mod services;

pub async fn run(auto_start: bool) {
    crate::service::reap_zombies_loop();
    let mut directories = vec![
        PathBuf::from("/Core/Services"),
        PathBuf::from("/Core/UserServices"),
        PathBuf::from("/Construct/Services"),
        PathBuf::from("/Space/*/.Services"), // all user home directories.
    ];
    if cfg!(debug_assertions) {
        directories = vec![PathBuf::from("./Services")]
    }
    if auto_start {
        for dir in directories {
            println!("{}", dir.display());
            //we need to traverse the directory and start all services found within recursively.
            if dir.exists() {
                for entry in walkdir::WalkDir::new(&dir) {
                    let entry = entry.unwrap();
                    let path = entry.path();
                    if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("rsc") {
                        //found a service file, start it.
                        let service_name = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unknown");
                        println!("Found service: {} at {}", service_name, path.display());
                        /*
                        * TODO:
                        * - check deadlocks incase of cyclic graph dependencies between services - and fail immediately
                        * - implement correct execution sequence, so if Service A needs the database (Service D), Service D must be executed and reach the Running state first.
                        * - set up environment variables
                        * - set up working directory
                        * - handle restart policies
                        * - handle scheduling if applicable
                        * - monitor the process and restart if needed
                        * - signal handling for graceful shutdown
                        * - cgroup setup for resource management
                        * - When a command arrives (Start ServiceX), the manager: a. Creates a Job (e.g., START) and checks the DAG for conflicts.
                        * b. If valid, it executes the job: fork() the manager process, and the child process executes execve() to launch the actual service binary.
                        * The Wait Loop (Zombie Reaping): The loop is constantly waiting for the SIGCHLD signal.
                            When a service process dies, the kernel sends SIGCHLD to PID 1.
                            PID 1 must immediately call the waitpid() syscall on the terminated process to remove its entry from the process table, preventing a zombie process.
                            This is one of the most critical responsibilities of PID 1.
                        */
                        crate::service::start_service_from_path(path);
                    }
                }
            } else {
                println!("Directory {} does not exist, skipping.", dir.display());
            }
        }
    }
    //implement socket server to listen for incoming commands to start/stop services.
    server::run().await.expect("Failed to run server");
}
