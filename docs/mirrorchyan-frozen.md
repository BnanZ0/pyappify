# PyInstaller 本体与 Mirror 完整包

应用使用项目自己的 `packaging/build.ps1` / `.spec` 构建 PyInstaller onedir。
PyAppify 在完整构建后记录所有本体文件并生成两个 ZIP；Git/setup/pip 路线保留。

## 产物布局

本体 ZIP `<应用名>-win-x86_64-v<版本号>-body.zip`：

```text
ok-nte.exe
_internal/                # Python、库、DLL 及库自身资源
assets/ icons/ i18n/ ...   # 所有最终构建中的外置应用资源
mid_lib/public/
README.md SPONSOR.md LICENSE
pyappify.yml              # 应用配置，不含本地状态
pyappify-files.json       # 相对本体根目录的程序文件清单
```

Mirror ZIP `<应用名>-win-x86_64-v<版本号>-full.zip`：

```text
<启动器>.exe              # 更新事务始终跳过
pyappify.yml
pyappify-release.json
data/apps/ok-nte/working/  # 与本体 ZIP 相同的内容
```

`app.json`、更新基线、个人数据与本地偏好不进入发布包。资源仍在应用 EXE 旁；
Mirror 直接启动本体 EXE，不需要外部 Python 或 pip。

## 构建入口

应用配置：

```yaml
mirrorchyan:
  resource_id: YOUR_RESOURCE_ID
  stable_channel: stable
  packaging: packaging
```

打包目录包含唯一 `.spec` 和 `requirements-build.txt`（兼容 `requirements.txt`）。
spec 文件名对应 `dist/<名称>/<名称>.exe`。有 `build.ps1` 时优先执行，以完成外置资源复制；
否则使用应用 `.venv` 执行 spec。两种方式完成后进入同一清单与 ZIP 实现。

```powershell
pyappify.exe -c mirror-zip '<app.yml>' '<Mirror RID>' '<应用版本>' '<mirror.zip>' ['<body.zip>']
```

未指定 body 输出时，`<名称>-full.zip` 对应 `<名称>-body.zip`（兼容旧的 `-mirror-full` 后缀）；其他名称追加 `-body`。
Action 文件名使用应用发布 tag 的版本，已有 `v` 前缀时不会重复添加；Windows x64 架构标记为 `x86_64`。
命令只构建一次应用，先生成本体 ZIP，再复用其压缩内容包装完整 ZIP。
JSON 结果保留 `zip`、SHA-256 等字段，新增 `body_zip` 和 `body_files`。

Action 的 `package_mode` 保持 `setup` / `mirror` / `all`，默认 `setup`。
Mirror 模式生成两种 ZIP，分别通过 `mirror-zip-path`、`body-zip-path` 输出，可独立上传。
不增加清单开关、程序路径列表或应用项目维护参数。

## 包装已有干净构建

在本仓库 `src-tauri` 中：

```powershell
cargo build --locked --release --example mirror_frozen_pack
.\target\release\examples\mirror_frozen_pack.exe `
  '<干净 onedir 的绝对路径>' '<启动器的绝对路径>' '<app.yml>' '<应用.exe>' `
  '<Mirror RID>' '<版本>' '<mirror.zip>' ['<body.zip>']
```

工具将应用 YAML 和自动清单写入输入构建目录，并收集完整目录，包括构建脚本复制的任意外置资源。
输入必须是未运行的发布构建；不从已经运行的用户安装收集文件。
`bundle_paths`、`preserve_paths` 不再用于新包生成或用户文件迁移。输出路径必须新建且在本体目录外。

## 清单与发布元数据

本体 `pyappify-files.json`：

```json
{"format": 1, "files": ["_internal/python312.dll", "ok-nte.exe", "pyappify-files.json", "pyappify.yml"]}
```

实际列表自动包含全部最终构建文件；不使用 TOC、哈希或 `data/apps/.../working` 前缀。
完整 ZIP 根 `pyappify-release.json` 保持格式 2，记录应用、RID、版本、启动器、运行入口和 profiles。
`preserve_paths` 仅兼容旧元数据反序列化；新发布为空，由程序文件清单确定用户文件。

## 安装与兼容

切源先检查应用已停止，仍运行则提示先停止；随后改名备份旧 `working`，安装使用正式目录。
Mirror → Git 要修改的 Python/仓库也先备份，事务包在原 Git 安装流程外。
成功迁移用户文件并提交记录后清理旧程序；失败、取消或中断恢复旧目录与记录。
普通同源 Git 安装、更新、同步清理和回滚恢复原作者规则；版本更新从新 YAML 的 `default` profile
选择依赖，只有依赖指定或内容变化时同步 pip。

完整冻结包离线、无 Key 可启动。手动下载前检查 Key；Mirror 取消恢复成功回到 Idle。
失败状态按原规则阻止启动；Git 启动时重试持久化的失败或中断目标。Mirror 失败需用户重试。
旧 Mirror 包缺少有效本体清单时无法可靠区分程序和用户文件，完整替换/切源会拒绝并保留旧安装。
可从同版本干净完整包恢复清单；不能扫描运行目录伪造清单。

启动权限、快捷方式和环境变量仍沿用现有流程。管理员 EXE 可能要求启动器也以管理员权限运行。
真实 Mirror 下载与实际应用联调范围见 [验证记录](mirrorchyan-source-switch-results.md)。
