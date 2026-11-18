"""从清华 TUNA 镜像抓取 mingw64 工具链（按 pacman 依赖递归解析）。

把 gcc/binutils/crt/headers 及其运行库的 .pkg.tar.zst 下载并解到
target/mingw64root/，让 gcc.exe / windres.exe 能直接跑起来。
"""
import os
import sys
import tarfile
import urllib.request

import zstandard

BASE = "https://mirrors.tuna.tsinghua.edu.cn/msys2/mingw/mingw64"
ROOT = os.path.join("target", "mingw64root")
WANT = ["mingw-w64-x86_64-gcc", "mingw-w64-x86_64-binutils"]

opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def fetch(url):
    with opener.open(url, timeout=180) as r:
        return r.read()


def parse_desc(text):
    fields, key = {}, None
    for line in text.splitlines():
        if line.startswith("%") and line.endswith("%"):
            key = line.strip("%")
            fields[key] = []
        elif line.strip() and key:
            fields[key].append(line)
    return fields


def load_db(work):
    db = os.path.join(work, "mingw64.db")
    if not os.path.exists(db):
        print("  下载索引...")
        with open(db, "wb") as f:
            f.write(fetch(f"{BASE}/mingw64.db"))
    entries = {}
    dctx = zstandard.ZstdDecompressor()
    with open(db, "rb") as raw:
        with dctx.stream_reader(raw) as reader:
            with tarfile.open(fileobj=reader, mode="r|") as tar:
                for member in tar:
                    if not member.name.endswith("/desc"):
                        continue
                    fields = parse_desc(tar.extractfile(member).read().decode("utf-8", "replace"))
                    name = (fields.get("NAME") or [""])[0]
                    if not name:
                        continue
                    entries[name] = {
                        "version": (fields.get("VERSION") or [""])[0],
                        "arch": (fields.get("ARCH") or ["any"])[0],
                        "depends": [d.split("=")[0].split("<")[0].split(">")[0]
                                    for d in (fields.get("DEPENDS") or [])],
                    }
    return entries


def resolve(entries, roots):
    seen, queue = set(), list(roots)
    while queue:
        name = queue.pop()
        if name in seen:
            continue
        if name not in entries:
            if name.startswith("mingw-w64-x86_64-"):
                print(f"  缺少依赖描述: {name}")
            continue
        seen.add(name)
        queue.extend(entries[name]["depends"])
    return sorted(seen)


def main():
    os.makedirs(ROOT, exist_ok=True)
    print("读取仓库索引...")
    entries = load_db(ROOT)
    print(f"  包总数 {len(entries)}")
    packages = resolve(entries, WANT)
    print(f"需要 {len(packages)} 个包")

    cache = os.path.join(ROOT, "_pkg")
    os.makedirs(cache, exist_ok=True)
    total, todo = 0, []
    for name in packages:
        meta = entries[name]
        filename = f"{name}-{meta['version']}-{meta['arch']}.pkg.tar.zst"
        path = os.path.join(cache, filename)
        if not (os.path.exists(path) and os.path.getsize(path) > 0):
            with open(path, "wb") as f:
                f.write(fetch(f"{BASE}/{filename}"))
        total += os.path.getsize(path)
        todo.append((filename, path))
        print(f"  {os.path.getsize(path)//1024:>7} KB  {filename}")
    print(f"合计 {total/1024/1024:.1f} MB")

    print("解压...")
    for filename, path in todo:
        dctx = zstandard.ZstdDecompressor()
        with open(path, "rb") as raw, dctx.stream_reader(raw) as reader:
            with tarfile.open(fileobj=reader, mode="r|") as tar:
                for member in tar:
                    member.uid = member.gid = 0
                    try:
                        tar.extract(member, ROOT, filter="data")
                    except Exception as exc:  # noqa: BLE001
                        print(f"  跳过 {member.name}: {exc}")
    print("完成 ->", os.path.abspath(ROOT))


if __name__ == "__main__":
    main()
