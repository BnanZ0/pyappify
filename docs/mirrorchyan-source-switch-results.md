# 本体清单、双 ZIP 与同目录切源

日期：2026-10-04。原作者行为基准为 `b1a5857de1c3e51d0058294aa6da2efecd6e2f02`（`52bc6d9` 的父提交）；保留用户 `4d06ad7` 的取消功能及其他未涉及的已有定制。本次目的仅为接入 Mirror 更新源。

## 实现

- 完整冻结应用可离线、无 Key 启动。下载前检查 Key；自动更新缺少 Key 不改写偏好。
- 选择的更新源和已提交安装记录分别保存。切换选择不删旧安装，切回旧源即可使用。
- Mirror → Git 先检查应用已停止，运行中拒绝安装并提示先停止；随后改名备份 `working`，在空的正式目录运行原 Git 安装流程。要修改的 `python`、`repo` 也先改名备份；pip 直接使用正式路径。
- Git → Mirror 在下载前检查应用已停止，校验和暂存完成后再次检查，再备份旧 `working`、放入冻结本体。提交成功才清理旧 Python、仓库和程序文件。
- 两种方向复用原有恢复日志。失败或取消删除新目录并还原目录、安装记录及 Mirror 根元数据；启动器中断后在启动时恢复。
- Mirror 操作取消并恢复成功回到 Idle、清空重试目标，显示普通取消完成；不持久化屏蔽某个版本。Git/pip 的取消检查、清理和状态处理沿用用户原实现。
- 原失败状态继续阻止启动和自动启动。Git 在启动时重试持久化的失败/中断目标；当前 Git 发布版本缺失时继续强制更新。Mirror 失败由用户重试。
- 普通同源 Git 的安装、更新、文件同步/清理、profile 的 `default` 回退和 pip 回滚恢复原实现；不复制 Python、不搬移 pip 入口或 editable 路径。
- 普通命令执行器不读取安装取消标志或强杀子进程树。Mirror 取消限定于下载、解压、安装准备；原 NSIS 的 `taskkill /F /T /PID` 和主动停止应用的提权后备 `taskkill /F /PID` 保持。
- Git 缺少 Python 时沿用原重置清理规则；仓库缺失按原规则标记未安装。仅 Mirror 冻结启动例外允许没有外部 Python。

### 保留的必要接线

1. 安装、更新与回滚保存实际安装记录，区分所选源和当前文件所属源，避免仅切换设置就错判已安装。
2. Mirror → Git 的目录事务包在原 Git 安装流程外；只有切源需要指定目标版本时，在原复制/pip 步骤前接入 Git checkout。普通 Git 路径不使用此事务。
3. 冻结 EXE 使用独立启动入口跳过外部 Python 检查；通用 Git 启动入口继续要求 Python。
4. 下载、替换前和 RM 占用查询中检查应用已停止。原主动停止功能保持，更新不调用它自动关闭应用。

## 文件识别与迁移

共同实现位于 `program_files.rs`。Git 使用旧安装版本的 Git 跟踪文件；Mirror 使用本体根目录 `pyappify-files.json`。

程序文件被替换；其余用户新增文件迁移到新 `working`。新程序路径及其文件/目录冲突由新程序占用。`_internal`、外部 Python 和仓库属于程序结构，不迁移旧库。
不使用维护列表、文件哈希、扩展名或修改时间识别用户文件，也不保留用户对已发布程序文件的修改。
该识别规则用于切源及 Mirror 迁移。普通 Git 恢复原复制工具、Git ignore 过滤及额外文件清理规则，未以 Git 跟踪清单替换所有普通同步。Mirror 增量删除程序目录时仅移除已登记程序文件，目录中的用户文件继续保留。

## 构建与产物

`build.ps1`、直接 spec 和已有干净 onedir 均进入同一打包实现。构建结束并复制所有外置资源后，PyAppify 自动生成 `pyappify-files.json`；应用项目不需要修改脚本、spec 或维护资源路径参数。

同一次应用构建先生成 `<应用名>-win-x86_64-v<版本号>-body.zip`，再复用其压缩内容生成 `<应用名>-win-x86_64-v<版本号>-full.zip`。
本体 ZIP 解压根目录直接有应用 EXE、`_internal`、外置资源、应用 YAML 和清单；不含启动器、`data/apps/.../working` 包装层或本地状态。
完整 ZIP 的 `working` 内容与本体 ZIP 一致，清单路径相对本体根目录。CLI 可指定第二个输出路径，并返回 `body_zip`；Action 通过 `body-zip-path` 暴露独立本体。

## 兼容限制

现有 `target/mirror-prune-validation/full.zip` 是 `v99.0.0-local.1` 测试包，不含清单，不能用于推断现场 `v1.4.6` 的程序文件。
旧 Mirror 安装没有有效清单时，完整替换和切源明确拒绝，保留原安装与未知文件；应从该版本的干净完整发布 ZIP 取得本体文件清单。不会扫描已运行安装伪造清单，也不会向远端请求历史清单。
手动包装入口必须提供从未运行过的干净构建目录，不能把用户安装目录重新打成发布包。

同源 Git 沿用原 pip 同步与失败 marker；文件和版本会恢复，但不保证回滚已部分修改的 Python 依赖。切源时 Python 目录有完整备份。

## 验证记录

本轮回归：85 项通过、0 项失败、2 项作为子进程夹具入口忽略，耗时 14.08 秒。覆盖运行应用拒绝更新且进程/文件保持不变、失败阻止启动、缺失 Git 版本仍按原策略强制更新，以及保留的 Mirror 下载、增量、切源、取消与恢复用例。

Git 六项逐段历史比对通过：安装/pip、工作目录同步清理、profile 回退、依赖内容选择、更新/pip 和 Git 回滚。比对仅排除格式空白、必要的实际安装记录保存及切源 checkout 接线，记录见 [git-policy-comparison.json](D:/vscode/pyappify/src-tauri/target/mirror-policy-review/git-policy-comparison.json)。

前端构建通过，耗时 4 分 28 秒，保留现有 chunk 体积及 resolve plugin 耗时提示。Release 启动器及两个 example 编译通过，耗时 10 分 22 秒；有一条 `ready_to_start` 仅在测试中调用的 dead code 提示。

[最终启动器](D:/vscode/pyappify/src-tauri/target/release/pyappify.exe)：11,527,168 字节，SHA-256 `327CA5C54F80E3589E4A15C720C44BF0ECD811A591061B566AEF911BFC7ADA31`。
已核对二进制嵌入本轮前端 `index-D41olbJq.js`，记录见 [launcher-verification.json](D:/vscode/pyappify/src-tauri/target/mirror-policy-review/launcher-verification.json)。

`cargo fmt --check`、主仓库和 Action 的 `git diff --check` 通过。Action 的 2 项已有检查、语法检查和 ncc 构建在前一轮通过；本轮保留 Action 代码。

性能计时、benchmark example 和纯格式调整属于独立辅助内容，保留已有工作，不作为改变原 Git/启动/取消策略的理由。

双 ZIP 检查使用历史干净 `v99.0.0-local.1` 测试本体，未启动应用；这些是结构验证产物，不是最新 OK-NTE 发布包。

| 验证产物 | 文件数 | ZIP 大小 |
| --- | ---: | ---: |
| [本体 ZIP](D:/vscode/pyappify/src-tauri/target/mirror-policy-review/body-manifest-validation/ok-nte-win32-body.zip) | 1,193 | 311.22 MiB |
| [Mirror 完整 ZIP](D:/vscode/pyappify/src-tauri/target/mirror-policy-review/body-manifest-validation/ok-nte-win32-mirror-full.zip) | 1,196 | 316.44 MiB |

两包清单一致；清单覆盖本体所有文件，路径相对本体根目录；EXE 位于本体 ZIP 根目录。本体不含启动器、包装层或本地状态；完整包的对应本体条目路径、原始大小及压缩大小全部匹配。完整包中的启动器 SHA-256 与最终编译结果一致。核对数据和 SHA-256 见 [verification.json](D:/vscode/pyappify/src-tauri/target/mirror-policy-review/body-manifest-validation/verification.json)。

现场 `O:/higame/ok-nte-win32-mirror-full` 和 OK-NTE 项目保持只读；没有验证最新 OK-NTE 构建或真实 CDK 下载联调，没有提交、推送或发布。
