use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use signal_hook::consts::signal::SIGCHLD;
use signal_hook::iterator::Signals;
use std::thread;

use crate::init::services;
use crate::parser::ServiceInfo;

pub fn reap_zombies_loop() {
    // Set up a signal handler for SIGCHLD
    let mut signals = Signals::new(&[SIGCHLD]).expect("Failed to create signal handler");

    thread::spawn(move || {
        for _ in signals.forever() {
            // Reap all dead children
            loop {
                /*
                 * TODO: When a service process exits, use the waitid() syscall with the P_PIDFD flag to wait on a specific pidfd.
                 * This is the most modern and efficient way to reap dead processes without blocking or involving the legacy waitpid() and its
                 * associated overheads.
                 */

                // FIXME: pass Pid::from_raw(-1) instead of -1 directly
                match waitpid(nix::unistd::Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => break,
                    Ok(WaitStatus::Exited(pid, status)) => {
                        println!("Child {} exited with status {}", pid, status);
                        // Update service info
                        services::update_service_pid(None, None, Some(pid.as_raw() as i32));
                    }
                    Ok(WaitStatus::Signaled(pid, signal, _core_dumped)) => {
                        println!("Child {} killed by signal {:?}", pid, signal);
                        // Update service info
                        services::update_service_pid(None, None, Some(pid.as_raw() as i32));
                    }
                    Ok(_) => {}
                    Err(nix::errno::Errno::ECHILD) => break, // No more children
                    Err(e) => {
                        eprintln!("waitpid error: {}", e);
                        break;
                    }
                }
            }
        }
    });
}

pub fn start_service_from_path(path: &std::path::Path) {
    //read the service config file from path
    let data = std::fs::read(path).expect("Failed to read service config file");
    let service_config: crate::parser::ServiceConfig =
        crate::parser::deserialize_service_config(&data);

    // Fix: clone Name before moving it into add_service
    let name = service_config.Name.clone();
    //check if name is already registered
    if services::get_service(&name).is_some() {
        eprintln!("Service {} is already running or registered.", name);
        return;
    } else {
        //register the service
        println!("Registering service: {}", name);
        services::register_service(
            name,
            ServiceInfo {
                Name: service_config.Name.clone(),
                IsRunning: false,
                Pid: None,
                LastExitCode: None,
                UpTimestamp: None,
                Config: service_config.clone(),
            },
        );
    }
    println!("Starting service: {:?}", service_config);
    // we cannot just spawn the process, we need to handle it properly similarly to how systemd does it.
    // fork a new process, and then child process does execve to start the actual service binary.
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Parent { child, .. }) => {
            // In the parent process
            println!("Started service with PID: {}", child);
            services::update_service_pid(
                Some(&service_config.Name),
                Some(child.as_raw() as i32),
                None,
            );
            // Here we would typically add the child PID to a tracking structure
            // to monitor its status and handle restarts based on the RestartPolicy.
        }
        #[allow(unreachable_code)]
        Ok(nix::unistd::ForkResult::Child) => {
            // TODO: Setup redirection of stdout/stderr to proper journaling system.
            // For now, we dont consume stdout/stderr of the child process, so throw it to /dev/null
            /*
            use std::fs::OpenOptions;
            use std::os::unix::io::AsRawFd;
            let devnull = OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .expect("Failed to open /dev/null");
            // Fix: Use libc::dup2 directly to avoid trait bound issues with nix::unistd::dup2
            unsafe {
                if libc::dup2(devnull.as_raw_fd(), libc::STDOUT_FILENO) == -1 {
                    eprintln!("Failed to redirect stdout");
                }
                if libc::dup2(devnull.as_raw_fd(), libc::STDERR_FILENO) == -1 {
                    eprintln!("Failed to redirect stderr");
                }
            }
            */

            //currently not redirecting stdout/stderr to see output in console for debugging

            // In the child process
            // Set up environment variables
            if !service_config.Env.is_empty() {
                for (key, value) in &service_config.Env {
                    unsafe {
                        std::env::set_var(key, value);
                    }
                }
            }
            // Change working directory if specified
            if let Some(ref dir) = service_config.WorkingDir {
                nix::unistd::chdir(dir).expect("Failed to change working directory");
            }
            // Execute the service binary
            use std::ffi::CString;

            let args = shell_words::split(&service_config.ExecStart)
                .expect("Failed to parse ExecStart command");

            if args.is_empty() {
                eprintln!("ExecStart command is empty");
                std::process::exit(1);
            }

            let exec_path_cstr = CString::new(args[0].clone())
                .expect("Failed to convert executable path to CString");

            let args_cstr: Vec<CString> = args
                .iter()
                .map(|arg| CString::new(arg.clone()).expect("Failed to convert arg to CString"))
                .collect();

            let args_ref: Vec<&std::ffi::CStr> = args_cstr.iter().map(|s| s.as_c_str()).collect();

            nix::unistd::execv(&exec_path_cstr, &args_ref)
                .expect("Failed to execute service binary");
            unreachable!("execv only returns on error");
        }
        Err(err) => {
            eprintln!("Fork failed: {}", err);
        }
    }
}
