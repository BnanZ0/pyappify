# Restart Manager 公共能力

Windows 实现位于 `src-tauri/src/restart_manager.rs`。`existing_files` 按调用方指定的
路径与深度枚举现存文件；`Session::new` 注册文件并设置保护；`affected_processes` 查询占用；
`shutdown` 由调用方选择正常或强制关闭；`restart` 由调用方显式恢复程序。
会话释放只调用 `RmEndSession`，不自动重启程序。公共层不决定应用更新模式，不替换文件。

## Mirror 占用检查与恢复策略

服务适配入口为 `app_service::release_zip_file_locks`，资源范围、应用运行检查及恢复策略
由该调用方决定。

1. 创建 RM 会话，按 PID 与创建时间保护当前启动器，并按 EXE 路径设置保护。
2. 再次核对已知 PID 的实际 EXE 路径；应用仍在运行就拒绝更新，提示先停止。
   注册现存文件，最多每批 256 个；不注册运行应用以请求关闭。
3. 调用 `RmGetList`。需要重启、权限错误、受保护进程或查询错误均使检查失败。
   调用方对占用列表再次核对进程路径，发现应用进程就拒绝更新。
   列表增长最多重查三次；不会无限重试。
4. 其他程序有占用时沿用 `RmShutdown`，flags 为 **0**；没有强制关闭、提权或强杀后备路径。
   取消请求通过 `RmCancelCurrentTask` 传给同步关闭操作。
5. 关闭返回后再次查询。仍有运行中的占用进程、保护冲突或重启要求就报失败。
   调用者只有获得成功会话后才开始事务文件替换。
6. 替换成功、失败或取消后，结束会话并尝试恢复 RM 关闭的其他已注册可重启程序。
   被更新应用和当前启动器设置 `RmNoShutdown`；应用自动启动
   由原有启动检查负责，RM 不绕过 profile、自动启动偏好及一次性启动参数。
   其他程序恢复失败记录提示，已提交更新仍按成功处理。

手动和自动更新均在下载前及替换前检查应用已停止，运行中拒绝更新，不自动关闭应用或轮询等待。
应用 EXE 通过 `RmNoShutdown` 防止在检查后重新启动时被 RM 关闭；其他占用程序正常关闭失败就终止本次更新。
ZIP 操作取消不调用强杀；Git/pip 的原取消行为保留。

微软说明的接口边界见 [RmShutdown](https://learn.microsoft.com/en-us/windows/win32/api/restartmanager/nf-restartmanager-rmshutdown)、
[RmAddFilter](https://learn.microsoft.com/en-us/windows/win32/api/restartmanager/nf-restartmanager-rmaddfilter)、
[RmRestart](https://learn.microsoft.com/en-us/windows/win32/api/restartmanager/nf-restartmanager-rmrestart)。

## 资源范围与事务边界

- **增量**：仅收集实际变更/删除的现存文件；删除目录只枚举其受影响子树。
  未变化的 lib 不遍历、不写入；新增且尚不存在的文件不注册。
- **完整替换**：注册 working 与 `_internal` 的直属文件（运行时、入口 EXE 等）、版本/配置
  文件。已知应用进程用于运行检查，不注册为关闭目标。不会递归扫描完整旧 lib，也不生成逐文件哈希或全目录清单。
- RM 注册的是文件，不把目录当作递归资源；批量注册减少重复登记开销。
  依据 [RmGetList](https://learn.microsoft.com/en-us/windows/win32/api/restartmanager/nf-restartmanager-rmgetlist)
  和 [RmRegisterResources](https://learn.microsoft.com/en-us/windows/win32/api/restartmanager/nf-restartmanager-rmregisterresources)。

全量策略无法提前发现所有旧库文件被第三方程序占用的情况。未登记资源、后续新占用、
权限或磁盘故障可能使目录移动/文件替换失败；调用者必须终止并回滚已完成动作。
RM 成功不构成文件替换一定成功的保证。事务暂存、备份、提交标记、中断恢复以及
已提交旧树的后台清理仍由 ZIP 文件应用代码负责。

## NSIS 接入

`installer.nsi` 将包内启动器释放到 `$PLUGINSDIR/restart-manager.exe`，通过
`--installer-restart-manager <安装目录> <安装器路径>` 调用相同的 Rust 公共模块。
该命令在 GUI、单实例及配置初始化之前执行。

安装器递归登记现存 `.exe`、`.dll`、`.pyd`，排除并保护安装器自身；沿用
`test-setup-1` 的正常关闭后强制关闭后备策略，不请求重新启动已关闭程序。
错误码返回 NSIS，交互安装保留重试，静默安装返回失败。

## 既有验证记录

以下为重构前的验证记录；本次接口调整尚未运行回归测试。

`cargo test --locked --lib -- --test-threads=1` 已通过以下专项用例：

- 增量资源仅含指明文件及删除子树；完整策略不枚举旧 lib。
- 当前进程被保护，RM 前置检查失败时没有负载替换。
- 模拟运行应用的隔离子进程使检查失败，进程和原文件保持不变。
- 模拟其他文件占用程序的隔离窗口子进程配合关闭，之后文件可以写入；测试 EXE 设置不重启。
- 子进程否决关闭时，检查失败、进程继续存活、负载字节保持不变。
- ZIP 验证另覆盖 RM 检查成功后新出现占用的执行阶段回滚。

隐藏窗口与进程均在工作区隔离目录中创建；测试结束仅清理测试子进程。
真实启动器 UI、管理员/跨用户程序、系统服务、真实 Mirror 资源联调尚未验证。
