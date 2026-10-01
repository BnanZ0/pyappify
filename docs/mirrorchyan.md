# MirrorChyan ZIP 更新准备

此分支从 `upstream/master`（`52bc6d9`）开始，按功能重新整理现有 master 的实现。
原 NSIS 实验完整保留在 master 的 `da0a87d`，可随时查阅。

## 当前已整理

1. 安装、Git/pip 更新与环境配置的取消操作，沿用原 `6fb2840`，并保留 master 中恢复安装控制台的取消入口。
2. MirrorChyan CDK 保存、更新源切换、最新版本/更新说明查询和更新 UI。

第二项直接取自现有 master 的实现：

- Windows DPAPI 加密 CDK，原子替换保存文件；不把 CDK 写入应用配置。
- 保留原安装路径对应的 `%LOCALAPPDATA%/PyAppify/updates/<路径摘要>/cdk.bin`，可读取原已保存的 CDK。
- 最新版本查询优先带 Windows 平台和架构；错误 8001 时回退到无平台标记的资源。
- 保留稳定/测试频道选择、空更新说明处理与不回显 CDK 的错误提示。
- 查询返回时检查更新源、配置和操作状态，避免旧结果覆盖正在进行的更新。
- 保留 Git 刷新时不持久化一次性启动参数的修复。
- MirrorChyan 安装不要求 Git 仓库；切回 Git 时先恢复原有仓库条件。
- 保留延迟启动前对取消请求、更新源、更新状态与应用运行情况的检查。
- 更新进度事件改为 `mirror-update-progress`；前端显示下载字节进度和未知进度阶段。

## 本提交的边界

ZIP 下载、增量应用和自动更新尚未接入。选择 MirrorChyan 后调用安装/更新会明确返回
`MirrorChyan ZIP installation is not connected yet`，不会执行 Git/pip 更新。
自动启动入口已保留，MirrorChyan 自动更新将在 ZIP 实现时接回。

没有引入旧 setup 下载器、安装 helper、安装结果回执、本地 NSIS 打包实验、依赖检查/卸载脚本。
上游自带的基础 NSIS 打包配置保持原样；此处分离的是 MirrorChyan 的 setup 更新链路。

Restart Manager 的旧实现仅在 NSIS 脚本中，目前没有独立的第三个提交。
ZIP 接入后若仍需要处理外部进程的文件占用，应针对变更/删除文件使用它，
避免扫描或注册整个 lib 目录，也需保护正在运行的启动器。

## 配置入口

应用的 `pyappify.yml` 可配置：

```yaml
mirrorchyan:
  resource_id: YOUR_RESOURCE_ID
  stable_channel: stable
  # 资源提供测试版时才启用：
  # prerelease_channel: beta
```

更新源由设置页面选择，默认仍为 `git`。CDK 在设置页面保存或清除。
此框架仓库没有写入 ok-nte 的资源 ID 或应用专属配置。

## 后续 ZIP 路线

- 同一资源 ID 分发完整应用 ZIP 与增量 ZIP，不再把 setup 作为 Mirro 更新负载。
- Mirro 可为已记录的任意旧版本到最新版生成直接增量，不限于上一版。
- 某版本组合的补丁未生成时，当前请求可能返回完整包并触发后台生成；客户端必须处理完整包回退。
- 增量包 `changes.json` 的路径相对包根目录，字段包括 `added`、`modified`、`deleted`、
  `added_dir`、`deleted_dir`，没有变更的类别可能省略。
- 未变化的 lib 不应读写；核心目标是依赖不变时更新在几秒内完成。
- Git/pip 安装或修改过的目录不能仅凭应用版本号认定为准确的 ZIP 增量基线。
- 尽量保持启动器运行，并保留更新、自动启动和用户偏好状态。

官方资料：[增量格式](https://github.com/MirrorChyan/docs/blob/main/Incremental.md)、
[上传 Action](https://github.com/MirrorChyan/uploading-action/blob/v1/action.yml)。
此前核对的后端版本是 `224169b563de488a7ea5fc21e49361be6e71f51d`，
其中 [最新版本选择](https://github.com/MirrorChyan/resource-backend/blob/224169b563de488a7ea5fc21e49361be6e71f51d/internal/logic/nv.go)
和 [补丁生成](https://github.com/MirrorChyan/resource-backend/blob/224169b563de488a7ea5fc21e49361be6e71f51d/internal/logic/version.go)
可以作为后续实现参考。
