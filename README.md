# Rev

Service Management System for RunixOS. Uses Rust for performance and safety, and MessagePack for efficient serialization.

Rev reads service configuration files from:

- `/Core/Services/`
- `/Core/UserServices/`
- `/Construct/Services/`
- `/Space/[username]/.Services/`

Here's a table chart that describes the differences between these directories:
| Directory                     | Purpose                          | Modify (Disable/Enable)  | Requires User Resources (Login, Desktop, Sound) | Create/Delete Permissions      |
|-------------------------------|----------------------------------|--------------------------|--------------------------------------------------|-------------------------------|
| `/Core/Services/`             | Core System-wide services            | Yes (Root)                       | No                                               | Not Allowed Unless Device Unlocked                     |
| `/Core/UserServices/`         | Core System-wide user services       | Yes (Root)                       | Yes                                              | Not Allowed Unless Device Unlocked                     |
| `/Construct/Services/`         | Modifiable System-wide services     | Yes (Root)                      | No                                               | Root only                     |
| `/Space/[username]/.Services/` | User-specific services          | Yes                      | Yes                                              | User only                     |

In order to understand which services are enabled/disabled, Rev uses a configuration file located at `/Construct/Config/rev.cfg` in MessagePack format. This file contains a list of services and their statuses (enabled/disabled). There's also a backup configuration file located at `/Core/ConfigBackup/rev.cfg`, which is used to restore the main configuration file if it becomes corrupted or lost.

Also, there's a config for Safe Mode located at `/Core/Config/rev_safe.cfg`, which is used to determine which services should be started when the system is booted into Safe Mode.

## Service file

Service name is in the format of `<vendor>.<app_name>.<function>` (eg. `rovelstars.files.indexer`).
You may make use of folders to organize your service files (eg. `rovelstars/files/indexer.service`).
Although discouraged, service files may omit the vendor name if the service is intended for personal use only (eg. `files.indexer`).
Similarly, the function part may be omitted if the service only has one function (eg. `rovelstars.files`).
And finally, the entire service name may be omitted if the service is only for personal use and has only one function (eg. `test`).

## TODOS:

- [ ] Implement service dependencies
- [ ] Implement service resource limits
- [ ] Implement service logging
- [ ] Implement service auto-restart on failure
- [ ] Implement service status monitoring
- [ ] Develop GUI & TUI for managing services.
- [ ] Write comprehensive documentation and user guides.
- [ ] Create unit and integration tests for Rev.
- [ ] Optimize performance for handling a large number of services.
- [ ] Implement security features to prevent unauthorized service modifications.