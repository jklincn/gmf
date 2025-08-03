# GMF – Get My File

## ✨ 简介

**GMF**（Get My File）通过 **Cloudflare R2** 中转，充分释放上传带宽、绕开运营商白名单限速，让你以接近带宽上限的速度安全下载远程主机文件。

## 🔑 核心特性

| 功能             | 说明                                             |
| ---------------- | ------------------------------------------------ |
| 🚀 **高速传输**   | 利用上传带宽 + Cloudflare 全球加速，突破限速瓶颈 |
| 🔐 **分块加密**   | 每个分块使用 AES-256-GCM 加密，兼顾效率与安全    |
| 🔄 **断点续传**   | 网络异常也能无缝恢复，传输更可靠                 |
| 🆓 **几乎零成本** | 充分利用 Cloudflare R2 免费额度，流量免费        |

## 📦 快速开始

1. **创建 R2 API 令牌**  

   - 参考 [Authentication · Cloudflare R2 docs](https://developers.cloudflare.com/r2/api/tokens/) 创建 R2 用户 API 令牌（注意选择「管理员读 + 写」权限）

2. **首次运行** 

   - 打开终端输入

     ```
     .\gmf.exe
     ```

   - 程序会在当前目录生成默认配置文件 `config.toml`

   - 填写远程主机和 R2 的连接信息

3. **下载文件**

   - 打开终端输入

     ```
     .\gmf.exe /remote/example.txt
     ```

## 🖥️ 命令行参数

```
Usage: gmf.exe [OPTIONS] <PATH>

Arguments:
  <PATH>  要下载的远程文件路径

Options:
  -c, --chunk-size <SIZE>  分块大小 [default: 10485760]
  -v, --verbose            打印详细输出
  -h, --help               Print help
  -V, --version            Print version
```

## 💰 Cloudflare R2 免费额度

如[官方定价](https://developers.cloudflare.com/r2/pricing/#free-tier)所示，Cloudflare 免费套餐为

| 项目                         | 免费额度（每月） |
| ---------------------------- | ---------------- |
| 存储                         | 10 GiB / 月      |
| A 类操作（主要是查询和上传） | 1,000,000 次     |
| B 类操作（主要是下载）       | 10,000,000 次    |
| **出口流量**                 | **免费**         |

GMF 通过 **分块上传/下载** 将存储占用控制在几十 MiB，同时利用 Cloudflare 对 R2 出口流量的 **免费政策**，几乎不产生任何费用。

## ⚠️ 常见问题

### Windows 提示 “Failed to lock memory for a cryptographic buffer”

```
Security warning: OS has failed to lock/unlock memory for a cryptographic buffer: VirtualLock: 0x5ad
This warning will only be shown once.
```

这是 `russh` 库在 Windows 上锁定加密缓冲区失败的提示，对传输功能影响不大，可放心使用。

跟踪 Issue ➜ https://github.com/Eugeny/russh/issues/504

## 🙏 致谢

- [Cloudflare R2](https://developers.cloudflare.com/r2/) – 免费出口流量的对象存储
- [russh](https://github.com/Eugeny/russh) – 轻量级 SSH/Rust 实现