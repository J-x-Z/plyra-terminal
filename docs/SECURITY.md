# Plyra Terminal 安全模型

## 会话与权限

普通窗口、终端解析器、渲染器和系统面板使用登录用户身份。普通 Shell 继承该账户的操作系统权限。用户在 Shell 中主动执行 sudo 等命令时，提权范围属于该 Shell 的进程树。结构化系统操作由独立的 `plyra-privilege-broker` 处理，使用单独的授权票据和审计记录。

普通 Broker 可向同 UID 且身份匹配的进程发送 SIGTERM，并重启当前账户 `gui/UID` 域中的 launchd 服务。管理员 Broker 可在用户确认后请求停止其他 UID 的指定进程。PID 0、PID 1、调用 UI 和 Broker 自身受到目标策略保护。服务重启范围始终绑定调用用户的 GUI 域。macOS SIP、TCC 和内核权限检查继续决定操作结果。

实现位于 `crates/privilege-policy/src/lib.rs` 和 `crates/system-api/src/backend.rs`。

## 调用者认证与确认

UI 启动普通 Broker 时，通过独立 stdin 管道提供随机 256 位 nonce、预期 UID/PID 和私有 Unix socket 路径。临时目录权限为 0700，socket 权限为 0600。macOS 使用 `getpeereid` 和 `LOCAL_PEERPID` 验证连接双方，Linux 使用 `SO_PEERCRED`。

每个连接使用递增序号。nonce、序号、消息长度或严格 JSON Schema 校验失败时，连接关闭，待确认操作随会话释放。单条消息上限为 64 KiB。`spawn` 完成 nonce 认证的身份 RPC 后返回，UI 获得的身份包含 Broker 实际 UID/PID、调用者 UID/PID 和管理员状态。

准备操作生成绑定目标的单次票据。服务端使用单调时钟限制 30 秒有效期，并在执行前重新读取目标身份。进程身份包含 PID、macOS 微秒启动标识、UID 和可执行文件路径。确认消费票据，撤销清空待确认票据。管理员撤销还关闭该 Broker 会话。

活动会话绑定原 UI PID。SSH 进程、终端子进程与插件具有各自的内核 PID。终端解析 crate 与会话 crate 的接口处理文本、屏幕和输入，系统操作入口位于应用系统面板。

实现位于 `crates/system-api/src/ipc.rs`、`crates/system-api/src/broker_client.rs` 和 `crates/privilege-broker/src/lib.rs`。

## macOS 管理员入口

用户点击管理员授权入口后，应用请求 macOS 原生管理员授权。启动程序固定为应用可执行文件同目录的 Broker。脚本在 root 新建的 0700 临时目录中复制 Broker，校验签名及预先固定的 CodeDirectory hash，再以清空后的环境执行。

管理员 Broker 验证暂存目录及程序的 root 所有权、程序签名和运行代码身份。连接同时绑定原 UI 的 UID、PID 和实际运行 CodeDirectory hash。UI 验证服务端内核 UID 0 和预期运行代码 hash。打包程序、helper 与 Broker 使用 hardened runtime 签名。

授权作用于当前应用会话。撤销关闭当前管理员 Broker，macOS 管理系统授权凭据的缓存和生命周期。此入口使用一次性进程及临时目录。

实现位于 `crates/system-api/src/identity.rs`、`crates/system-api/src/broker_client.rs` 和 `tools/build_macos_app.py`。

## 审计与观测范围

Broker 在执行系统写操作前同步写入授权记录，并追加执行结果。认证拒绝、目标策略拒绝、过期或已消费票据、撤销及身份读取均有记录。字段包含时间、请求 UID/PID、操作、目标和结果。记录中的随机文件名独立于认证 nonce。

审计文件保存在当前用户 `Library/Application Support/GlassTerminal/audit` 私有目录。管理员记录保留 root 所有权，通过外层 0700 目录向该用户提供读取权限。持久审计 API 在会话撤销后仍可读取，单次返回至多 500 条，读取范围为最近 32 个会话文件，每文件上限 1 MiB。目录访问使用固定目录 FD 和 `O_NOFOLLOW`。这类本地记录的保留和可用性受账户对其数据目录的管理影响。

进程树与网络变化来自有时间戳的 OS 快照。采样间隔决定短生命周期变化的可见范围。接口计数为累计字节计数，连接详情通过限定的当前用户 lsof 查询获得。文件事件监听覆盖 managed 目录根层，队列上限为 512 条。统一日志查询要求进程或消息过滤条件，限制为 300 秒、500 条及固定输出字节预算。受保护进程路径和日志字段受系统读取权限约束。

实现位于 `crates/system-api/src/audit.rs`、`crates/system-api/src/backend.rs` 和 `apps/desktop/src/system_panel.rs`。

## 受控文件恢复

文件编辑与恢复覆盖 managed 目录内的单个文件名，名称上限为 255 字节，文件上限为 1 MiB。目录 FD 固定文件操作边界，打开文件使用 `O_NOFOLLOW`，常规文件和单硬链接检查限制可编辑对象。journal 保存修改前内容、SHA-256、目标路径和事务状态。

同一 managed 目录的 API 事务通过跨实例文件锁串行化。修改与恢复分别先持久化意图，再原子替换目标，最后记录完成状态。恢复校验当前内容 hash，检测后续修改。中断的恢复可根据意图和当前 hash 重建状态，已完成恢复支持安全重试。当前保留预算为 128 个事务，已有快照保留在 journal 中。

恢复覆盖所跟踪文件的内容状态。网络、设备、进程和内核操作保留各自的操作系统执行结果。

实现位于 `crates/system-api/src/recovery.rs` 和 `apps/desktop/src/extensions.rs`。

## 插件边界

macOS CLI 插件运行于 Seatbelt deny-default 配置中。基础能力允许读取指定程序、系统运行库及 managed 文件。写 managed 文件与网络访问分别由 UI 显式开启。journal 的读写、进程派生和 Broker 权限受到独立限制。

插件环境清空后只配置 PATH、LANG 以及指向 managed 目录的 HOME/TMPDIR，工作目录同样固定在 managed 目录。运行上限为 3 秒，每个输出流上限为 64 KiB。插件输出作为数据返回 UI。认证 nonce 与连接句柄由 UI 的私有 Broker 客户端持有。

实现位于 `crates/system-api/src/plugin.rs` 和 `crates/system-api/src/backend.rs`。

## MCP 连接与工具

MCP 使用固定版本的官方 Rust SDK `rmcp 3.9.0`。配置文件保存服务器、传输、超时和输出预算，界面在用户显式连接后启动 stdio 进程或建立 Streamable HTTP 连接。工具调用绑定已列出的工具名和完整 JSON 参数，单次随机票据使用单调时钟到期，消费后失效。界面显示参数并由用户点击调用。MCP 返回的内容作为数据显示。

stdio 服务器拥有登录账户可访问文件和网络的权限。Unix 客户端拒绝 UID 0，子进程独立进程组，Windows 使用 Job Object。连接关闭或最后一个客户端释放时执行有界关闭及进程清理。服务器环境经过清空，只继承固定的基础变量及 `env_refs` 显式指定的引用。HTTP Bearer 认证通过 `bearer_token_env` 在连接时读取，配置保存环境变量名。调试输出省略命令参数、HTTP URL、工具参数、票据和响应内容。

每个连接的请求队列、分页、单条消息和聚合结果均受配置预算约束。取消会撤销排队请求，向执行中的请求发送协议取消通知，并结束本地等待。已经发生的外部副作用由服务器处理。服务器过期会话恢复关闭，重新连接由用户发起。客户端声明的能力和协议处理器限制服务器发起的回调，MCP 客户端持有独立连接，系统 Broker 入口继续绑定原 UI 身份和授权票据。

实现位于 `crates/mcp-client` 和 `apps/desktop/src/mcp_panel.rs`。传输、生命周期和协议协商依据官方 SDK 与 MCP 规范。[Rust SDK](https://github.com/modelcontextprotocol/rust-sdk)、[MCP 规范](https://modelcontextprotocol.io/specification/2026-07-28)

## 验证状态

macOS 普通用户进程控制使用本任务创建的 sleep 子进程验证。nonce、防重放、其他进程 PID 拒绝、票据消费与撤销、持久恢复、文件链接拒绝、FSEvents、插件环境清理及沙盒边界具备定向测试。

管理员脚本经过编译检查，实际管理员授权与 UID 0 身份握手需要用户在原生授权界面完成验证。当前原生授权、launchd、统一日志、IOKit USB 与 Seatbelt 后端面向 macOS。其他平台的具体系统管理和插件隔离能力通过各自后端接入。
