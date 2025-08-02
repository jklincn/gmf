# GMF

**G**et **M**y **F**ile 是一款破解运营商白名单限速的文件传输工具。通过 Cloudflare R2 中转，可以让你高速地获取远程主机的文件。

GMF 具有以下特征：

- 充分利用上传带宽，绕开白名单限速
- 采用分块加密（AES-256-GCM）传输，高效同时保证数据安全
- 支持断点续传，无惧网络断开风险

## 使用

1. 创建 [Cloudflare R2 用户 API 令牌](https://developers.cloudflare.com/r2/api/tokens/)，**权限为管理员读和写**。
2. **打开终端运行程序**，首次运行将创建默认配置文件，修改远程服务器连接信息与 R2 API 令牌。
3. 再次**打开终端运行程序**，将远程服务器文件路径作为运行参数即可开始下载。

```
Usage: gmf.exe [OPTIONS] <PATH>

Arguments:
  <PATH>  要下载的远程文件路径

Options:
  -c, --chunk-size <SIZE>     分块大小 [default: 10485760]
  -n, --concurrency <NUMBER>  并发上传数 [default: 1]
  -v, --verbose               打印详细输出
  -h, --help                  Print help
  -V, --version               Print version
```

## Cloudflare R2 用量解释

如[官方定价](https://developers.cloudflare.com/r2/pricing/#free-tier)所示，Cloudflare 免费套餐为

|                              | 免费额度       |
| ---------------------------- | -------------- |
| 存储                         | 10 GB / 月      |
| A 类操作（主要是查询和上传） | 100 万次 / 月  |
| B 类操作（主要是下载）       | 1000 万次 / 月 |
| **出口流量**                 | **免费**       |

GMF 在运行时，通过分块上传/下载的方式将存储用量压到了最小，根据本地下载速度总用量在几十 MB 左右。同时得益于 Cloudflare 的善举，高速下载流量是免费的，因此使用 GMF 几乎产生不了任何成本。

## 已知问题

### 内存锁定失败的安全警告

```
Security warning: OS has failed to lock/unlock memory for a cryptographic buffer: VirtualLock: 0x5ad
This warning will only be shown once.
```

这个警告表示 russh 库在尝试锁定内存中的加密缓冲区时失败了，对程序没有非常严重的影响。

等待官方解决： [Log warning on windows · Issue #504 · Eugeny/russh](https://github.com/Eugeny/russh/issues/504)