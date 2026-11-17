# Rust 技术验证与终端后端复用

## 决定

在当前 GPL-3.0-only 仓库增加 Cargo workspace，沿用 Unix C/libvterm helper 的源码、协议和许可证。winit、wgpu、glyphon 负责新原生前端。portable-pty/vt100 提供跨架构共同后端与 Windows ConPTY 接入，输出共享 terminal-core 屏幕模型。

## 依据

G0 已核验独立后端来源、哈希和现有真实 macOS PTY 证据。Godot 的绘制与输入模块具有运行时耦合。现有后端不含 Windows 平台实现。保留原有入口能继续满足 Mobile 的固定版本同步调用。

## 约束

终端输出只进入有界显示模型。UI 以普通用户运行。G1 不安装系统服务或特权 Broker。主线程按事件重绘，IME 预编辑与提交分别处理。零大小窗口暂停 GPU surface，恢复后重新配置。Android 生命周期需要单独验证。

版本与许可证通过 Cargo.lock 及依赖清单记录。构建证据与平台功能证据分别标明状态。AetherLang/JXZ 作为后续普通权限扩展候选。
