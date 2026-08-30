#!/usr/bin/env python3
"""重建 `dist/cpe_test-vX.Y.Z-windows-config-docs.zip` 及其 .sha256。

这个包是**跟着仓库走**的：CI 会把它逐字节和源文件比对，对不上就红。
所以任何一次改了 README / 使用说明 / dist 下的 bat 或示例配置，都必须跑一遍
这个脚本，否则流水线会拦下发布。

打出来的包是**可复现**的：时间戳固定、压缩参数固定，同样的输入必然得到同样的
SHA-256。不这样的话每次重打都会换一个校验值，`.sha256` 就失去意义了。

用法：
    python3 dist/build_config_docs_bundle.py            # 版本号从 Cargo.toml 读
    python3 dist/build_config_docs_bundle.py 4.6.0      # 或显式指定
"""

import hashlib
import re
import sys
from pathlib import Path
from zipfile import ZIP_DEFLATED, ZipFile, ZipInfo

# 包内路径 -> 仓库里的源文件。顺序即包内顺序，改动请同步 .github/workflows/build.yml
# 里那份校验清单，两边必须一致。
LAYOUT = {
    "Windows配置与文档包说明.md": "dist/Windows配置与文档包说明.md",
    "README.md": "README.md",
    "README-Windows-快速开始.md": "dist/README-Windows-快速开始.md",
    "使用说明.md": "使用说明.md",
    "NIC_README.md": "NIC_README.md",
    "UDP并发灌包验收场景.md": "UDP并发灌包验收场景.md",
    "config.minimal.json": "config.minimal.json",
    "config.example.json": "config.example.json",
    "configs/config-sgmii.json": "dist/configs/config-sgmii.json",
    "configs/config-wifi5g.json": "dist/configs/config-wifi5g.json",
    "configs/config-10gusb.json": "dist/configs/config-10gusb.json",
    "configs/config-all-common.json": "dist/configs/config-all-common.json",
    "configs/config-full-tcp-udp-ping.json": "dist/configs/config-full-tcp-udp-ping.json",
    "projects/cpe-ui-project-full.json": "dist/projects/cpe-ui-project-full.json",
    "start_ui.bat": "dist/start_ui.bat",
    "start_agent.bat": "dist/start_agent.bat",
    "start_master.bat": "dist/start_master.bat",
    "start_master_select_config.bat": "dist/start_master_select_config.bat",
    "iperf3-请放到这里.txt": "dist/iperf3-请放到这里.txt",
    "ctsTraffic-请放到这里.txt": "dist/ctsTraffic-请放到这里.txt",
    "LICENSE-cpe_test-MIT.txt": "LICENSE",
    "THIRD_PARTY_NOTICES.md": "THIRD_PARTY_NOTICES.md",
}

# 固定时间戳：包要可复现，就不能让打包时刻混进校验值。
FIXED_TIME = (2026, 1, 1, 0, 0, 0)
# 0o100644 << 16：普通文件、rw-r--r--。跨平台重打时权限位不跟着环境变。
FILE_ATTR = 0o100644 << 16


def repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def version_from_cargo(root: Path) -> str:
    text = (root / "Cargo.toml").read_text(encoding="utf-8")
    match = re.search(r'^version\s*=\s*"([^"]+)"', text, re.M)
    if not match:
        raise SystemExit("Cargo.toml 里没找到 version")
    return match.group(1)


def main() -> int:
    root = repo_root()
    version = sys.argv[1] if len(sys.argv) > 1 else version_from_cargo(root)
    prefix = f"cpe_test-v{version}-windows-config-docs"
    bundle = root / "dist" / f"{prefix}.zip"

    missing = [src for src in LAYOUT.values() if not (root / src).is_file()]
    if missing:
        raise SystemExit("这些源文件不存在：\n  " + "\n  ".join(missing))

    # 批处理必须是 ANSI(GBK)：Windows 的 cmd 默认按 936 代码页解释脚本，
    # UTF-8 的中文会整片变成乱码，连报错信息都读不了。
    for name, src in LAYOUT.items():
        if not name.endswith(".bat"):
            continue
        raw = (root / src).read_bytes()
        try:
            raw.decode("gbk")
        except UnicodeDecodeError as error:
            raise SystemExit(f"{src} 不是 GBK/ANSI 编码，Windows 上会乱码：{error}")
        if b"\r\n" not in raw:
            raise SystemExit(f"{src} 不是 CRLF 换行，部分 Windows 环境会解析异常")

    with ZipFile(bundle, "w", ZIP_DEFLATED, compresslevel=9) as archive:
        for name, src in LAYOUT.items():
            info = ZipInfo(f"{prefix}/{name}", date_time=FIXED_TIME)
            info.compress_type = ZIP_DEFLATED
            info.external_attr = FILE_ATTR
            # 非 ASCII 文件名必须打 UTF-8 标记位，否则 Windows 解压出来是乱码目录。
            if not info.filename.isascii():
                info.flag_bits |= 0x800
            archive.writestr(info, (root / src).read_bytes())

    digest = hashlib.sha256(bundle.read_bytes()).hexdigest()
    checksum = bundle.with_suffix(".zip.sha256")
    checksum.write_text(f"{digest}  {bundle.name}\n", encoding="utf-8")

    print(f"已生成 {bundle.relative_to(root)}（{len(LAYOUT)} 项，{bundle.stat().st_size} 字节）")
    print(f"SHA-256 {digest}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
