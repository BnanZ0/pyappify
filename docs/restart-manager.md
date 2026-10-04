# Restart Manager 公共能力

Windows 实现位于 `src-tauri/src/restart_manager.rs`。`existing_files` 按调用方指定的
路径与深度枚举现存文件；`Session::new` 注册文件并设置保护；`affected_processes` 查询占用；
`shutdown` 由调用方选择正常或强制关闭；`restart` 由调用方显式恢复程序。
会话释放只调用 `RmEndSession`，不自动重启程序。公共层不决定应用更新模式，不替换文件。

## 服务适配与后续接线

`app_service::release_zip_file_locks` 按业务指定资源范围，检查应用进程，选择正常关闭，
并在关闭失败或取消后显式恢复其他程序。应用 EXE 与当前进程受保护。
自动更新发现应用运行时返回待处理状态，手动更新返回错误。

该提交提供公共能力及服务适配函数；ZIP 事务与实际调用接线由后续提交完成。
调用方负责在文件应用成功、失败或取消后请求恢复其他程序；NSIS 可选择自己的关闭策略。
公共模块不执行文件替换、提权或 `taskkill`。

## 资源与接口边界

注册的是文件；调用方决定递归范围。每批最多注册 256 个文件。
占用列表变化时最多查询三次，需要系统重启或受保护进程占用时返回错误。
关闭操作沿用 `RmCancelCurrentTask` 取消支持，返回后再次查询运行占用。
RM 成功不保证之后的文件替换成功，调用方仍负责处理失败与回滚。

既有原生窗口与资源范围测试随接口调整保留。本次未执行回归测试。

## NSIS 接入

`installer.nsi` 将包内启动器释放到 `$PLUGINSDIR/restart-manager.exe`，通过
`--installer-restart-manager <安装目录> <安装器路径>` 调用相同的 Rust 公共模块。
该命令在 GUI、单实例及配置初始化之前执行。

安装器递归登记现存 `.exe`、`.dll`、`.pyd`，排除并保护安装器自身；沿用
`test-setup-1` 的正常关闭后强制关闭后备策略，不请求重新启动已关闭程序。
错误码返回 NSIS，交互安装保留重试，静默安装返回失败。

