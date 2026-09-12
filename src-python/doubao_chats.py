# -*- coding: utf-8 -*-
"""doubao_chats.py — 豆包桌面端 IndexedDB（chrome_doubao-chat_0 等）解析与对话导出。

自包含实现，无第三方依赖：
  1. leveldb 目录读取：WAL（.log，CRC32C 校验逐记录重组）+ SST（.ldb，纯 Python snappy 解压），
     按 seq 合并、处理墓碑，跳过 LevelDBScopes 恢复日志与 blob journal；
  2. IndexedDB 键解码（KeyPrefix 位打包 + EncodeIDBKey：number/date 8B LE double、
     string varint 长度 + UTF-16BE、array 递归、binary varint 长度）——
     编码规则对齐 Chromium content/browser/indexed_db/indexed_db_leveldb_coding.cc；
  3. 值解码：剥离 IndexedDB value wrapper（[ver][FF 15 FE][12×00][FF 0F]）后做
     V8 SerializationFormat 反序列化（对齐 v8/src/objects/value-serializer.cc，
     除 kUtf8String 为 u32 LE 外，长度/计数均为 varint；double 为宿主序 LE）。

CLI：
  python doubao_chats.py --scan                 # 打印各库/对象存储摘要
  python doubao_chats.py --uid <uid> --export-md out.md --export-json out.json
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import sqlite3
import struct
import sys
import tempfile
import time
from pathlib import Path

# ─────────────────────────── 基础工具 ───────────────────────────


def _varint(b: bytes, i: int) -> tuple[int, int]:
    r = s = 0
    while True:
        if i >= len(b):
            raise ValueError("varint out of range")
        c = b[i]
        i += 1
        r |= (c & 0x7F) << s
        if not (c & 0x80):
            return r, i
        s += 7


_CRC32C_TABLE = []
for _i in range(256):
    _c = _i
    for _ in range(8):
        _c = (_c >> 1) ^ 0x82F63B78 if _c & 1 else _c >> 1
    _CRC32C_TABLE.append(_c)


def _crc32c(data: bytes) -> int:
    crc = 0xFFFFFFFF
    for b in data:
        crc = _CRC32C_TABLE[(crc ^ b) & 0xFF] ^ (crc >> 8)
    return crc ^ 0xFFFFFFFF


def _mask_crc(c: int) -> int:
    return (((c >> 15) | (c << 17)) + 0xA282EAD8) & 0xFFFFFFFF


def _snappy_decompress(src: bytes) -> bytes:
    i = 0
    ulen = 0
    shift = 0
    while True:
        c = src[i]
        i += 1
        ulen |= (c & 0x7F) << shift
        shift += 7
        if not (c & 0x80):
            break
    out = bytearray()
    n = len(src)
    while i < n:
        t = src[i]
        i += 1
        tag = t & 3
        if tag == 0:
            ln = (t >> 2) + 1
            if ln > 60:
                nb = ln - 60
                ln = int.from_bytes(src[i : i + nb], "little") + 1
                i += nb
            out += src[i : i + ln]
            i += ln
        else:
            if tag == 1:
                ln = ((t >> 2) & 7) + 4
                off = ((t >> 5) << 8) | src[i]
                i += 1
            elif tag == 2:
                ln = (t >> 2) + 1
                off = int.from_bytes(src[i : i + 2], "little")
                i += 2
            else:
                ln = (t >> 2) + 1
                off = int.from_bytes(src[i : i + 4], "little")
                i += 4
            if off == 0:
                raise ValueError("snappy offset 0")
            for _ in range(ln):
                out.append(out[-off])
    return bytes(out)


# ─────────────────────── leveldb 记录读取 ───────────────────────


def _read_wal_batches(raw: bytes) -> list[bytes]:
    """32KB 块 + [crc4][len2][type1] 记录；type 1=full 2=first 3=middle 4=last。"""
    pos = 0
    buf = b""
    batches = []
    while pos + 7 <= len(raw):
        off = pos % 32768
        if off + 7 > 32768:  # 记录头不跨块
            pos += 32768 - off
            continue
        length = struct.unpack("<H", raw[pos + 4 : pos + 6])[0]
        rtype = raw[pos + 6]
        payload = raw[pos + 7 : pos + 7 + length]
        pos += 7 + length
        if rtype in (1, 4):
            batches.append(buf + payload)
            buf = b""
        elif rtype in (2, 3):
            buf += payload
    return batches


def _read_sst_entries(raw: bytes) -> list[tuple[int, int, bytes, bytes]]:
    """SST → [(seq, type, user_key, value)]；value 为空串表示删除。"""
    def varint_l(b, i):
        return _varint(b, i)

    i = len(raw) - 48
    off, i = _varint(raw, i)
    size, i = _varint(raw, i)
    off2, i = _varint(raw, i)
    size2, i = _varint(raw, i)
    idx = raw[off2 : off2 + size2]
    n_rs = struct.unpack("<I", idx[-4:])[0]
    j = 0
    end = len(idx) - 4 - n_rs * 4
    last = b""
    blocks = []
    while j < end:
        sh, j = _varint(idx, j)
        ns, j = _varint(idx, j)
        vl, j = _varint(idx, j)
        key = last[:sh] + idx[j : j + ns]
        j += ns
        last = key
        val = idx[j : j + vl]
        j += vl
        o = 0
        ob, o = _varint(val, o)
        sb, o = _varint(val, o)
        blocks.append((ob, sb))
    out = []
    for ob, sb in blocks:
        comp = raw[ob + sb]
        blk = _snappy_decompress(raw[ob : ob + sb]) if comp == 1 else raw[ob : ob + sb]
        n_rs2 = struct.unpack("<I", blk[-4:])[0]
        end2 = len(blk) - 4 - n_rs2 * 4
        j = 0
        last = b""
        while j < end2:
            sh, j = _varint(blk, j)
            ns, j = _varint(blk, j)
            vl, j = _varint(blk, j)
            key = last[:sh] + blk[j : j + ns]
            j += ns
            last = key
            val = blk[j : j + vl]
            j += vl
            ukey, tag = key[:-8], struct.unpack("<Q", key[-8:])[0]
            out.append((tag >> 8, tag & 0xFF, ukey, val))
    return out


def read_leveldb_dir(db_dir: Path) -> dict[bytes, tuple[int, bytes]]:
    """读一个 IndexedDB leveldb 目录，返回 {key: (seq, value)}（已应用删除）。"""
    best: dict[bytes, tuple[int, bytes]] = {}

    def put(seq: int, key: bytes, val: bytes | None) -> None:
        cur = best.get(key)
        if cur is None or seq > cur[0]:
            best[key] = (seq, val if val is not None else b"")

    for fp in sorted(db_dir.glob("*.ldb")):
        try:
            for seq, t, key, val in _read_sst_entries(fp.read_bytes()):
                put(seq, key, val if t == 1 else None)
        except Exception:
            continue  # 损坏文件跳过

    for fp in sorted(db_dir.glob("*.log")):
        try:
            raw = fp.read_bytes()
        except Exception:
            continue
        for batch in _read_wal_batches(raw):
            if len(batch) < 12:
                continue
            seq0 = struct.unpack("<Q", batch[:8])[0]
            count = struct.unpack("<I", batch[8:12])[0]
            i = 12
            try:
                for n in range(count):
                    t = batch[i]
                    i += 1
                    klen, i = _varint(batch, i)
                    key = batch[i : i + klen]
                    i += klen
                    if t == 1:
                        vlen, i = _varint(batch, i)
                        put(seq0 + n, key, batch[i : i + vlen])
                        i += vlen
                    else:
                        put(seq0 + n, key, None)
            except (IndexError, ValueError):
                continue  # 尾部半截批次
    return best


# ─────────────────────── IndexedDB 键解码 ───────────────────────

IDB_NULL, IDB_STRING, IDB_DATE, IDB_NUMBER, IDB_ARRAY, IDB_MIN, IDB_BINARY = range(7)


def decode_prefix(key: bytes) -> tuple[int, int, int, bytes]:
    """KeyPrefix：首字节 = (db_size-1)<<5 | (os_size-1)<<2 | (idx_size-1)。"""
    fb = key[0]
    db_sz = (fb >> 5) + 1
    os_sz = ((fb >> 2) & 7) + 1
    idx_sz = (fb & 3) + 1
    p = 1
    db = int.from_bytes(key[p : p + db_sz], "little")
    p += db_sz
    os_ = int.from_bytes(key[p : p + os_sz], "little")
    p += os_sz
    idx = int.from_bytes(key[p : p + idx_sz], "little")
    p += idx_sz
    return db, os_, idx, key[p:]


def decode_idb_key(b: bytes) -> object:
    t = b[0]
    if t == IDB_NULL or t == IDB_MIN:
        return None
    if t == IDB_STRING:
        n, i = _varint(b, 1)
        return b[i : i + n * 2].decode("utf-16-be", errors="replace")
    if t in (IDB_NUMBER, IDB_DATE):
        import struct as _s

        v = _s.unpack("<d", b[1:9])[0]
        return v
    if t == IDB_ARRAY:
        n, i = _varint(b, 1)
        out = []
        for _ in range(n):
            start = i
            _sub = _idb_key_span(b, i)
            i = _sub[1]
            out.append(decode_idb_key(b[start:i]))
        return out
    if t == IDB_BINARY:
        n, i = _varint(b, 1)
        return bytes(b[i : i + n])
    raise ValueError(f"idb key type {t}")


def _idb_key_span(b: bytes, i: int) -> tuple[int, int]:
    t = b[i]
    if t in (IDB_NULL, IDB_MIN):
        return i, i + 1
    if t == IDB_STRING:
        n, j = _varint(b, i + 1)
        return i, j + n * 2
    if t in (IDB_NUMBER, IDB_DATE):
        return i, i + 9
    if t == IDB_BINARY:
        n, j = _varint(b, i + 1)
        return i, j + n
    if t == IDB_ARRAY:
        n, j = _varint(b, i + 1)
        for _ in range(n):
            _, j = _idb_key_span(b, j)
        return i, j
    raise ValueError(f"idb key type {t}")


# ─────────────────────── V8 序列化反解 ───────────────────────


class _V8:
    __slots__ = ("b", "i", "ids")

    def __init__(self, b: bytes):
        self.b = b
        self.i = 0
        self.ids: list = []

    def _read_varint(self) -> int:
        v, self.i = _varint(self.b, self.i)
        return v

    def _u32le(self) -> int:
        v = struct.unpack("<I", self.b[self.i : self.i + 4])[0]
        self.i += 4
        return v

    def _double(self) -> float:
        v = struct.unpack("<d", self.b[self.i : self.i + 8])[0]
        self.i += 8
        return v

    def value(self):
        tag = self.b[self.i]
        self.i += 1
        b, i = self.b, self.i
        if tag == 0xFF:  # version
            self._read_varint()
            return self.value()
        if tag == 0:  # padding
            return self.value()
        if tag == 0x3F:  # '?' verify object count
            self._u32le()
            return self.value()
        if tag == 0x2D:  # '-' hole
            return None
        if tag == 0x5F:  # '_' undefined
            return None
        if tag == 0x30:  # '0' null
            return None
        if tag == 0x54:  # T
            return True
        if tag == 0x46:  # F
            return False
        if tag == 0x49:  # 'I' int32 zigzag
            v = self._read_varint()
            return (v >> 1) ^ -(v & 1)
        if tag == 0x55:  # 'U' uint32
            return self._read_varint()
        if tag == 0x4E:  # 'N' double
            return self._double()
        if tag == 0x53:  # 'S' utf8 (u32 LE length)
            n = self._u32le()
            s = self.b[self.i : self.i + n].decode("utf-8", errors="replace")
            self.i += n
            return s
        if tag == 0x22:  # '"' one-byte
            n = self._read_varint()
            s = self.b[self.i : self.i + n].decode("latin-1")
            self.i += n
            return s
        if tag == 0x63:  # 'c' two-byte UTF-16LE
            nb = self._read_varint()
            s = self.b[self.i : self.i + nb].decode("utf-16-le", errors="replace")
            self.i += nb
            return s
        if tag == 0x5E:  # '^' object reference
            ref = self._read_varint()
            if ref < len(self.ids):
                return self.ids[ref]
            return None
        if tag == 0x6F:  # 'o' begin object
            obj: dict = {}
            self.ids.append(obj)
            while True:
                t2 = b[self.i]
                if t2 == 0x7B:  # '{' end
                    self.i += 1
                    self._read_varint()  # num properties
                    return obj
                k = self.value()
                v = self.value()
                obj[k if not isinstance(k, float) else str(k)] = v
        if tag == 0x61:  # 'a' sparse array（起始即带 varint 长度）
            self._read_varint()
            arr: dict = {}
            self.ids.append(arr)
            length = 0
            while True:
                t2 = b[self.i]
                if t2 == 0x40:  # '@' end
                    self.i += 1
                    self._read_varint()
                    self._read_varint()
                    out = [None] * (length if isinstance(length, int) else 0)
                    for k, v in arr.items():
                        try:
                            out[int(k)] = v
                        except Exception:
                            pass
                    return out
                k = self.value()
                v = self.value()
                arr[k] = v
                try:
                    length = max(length, int(k) + 1)
                except Exception:
                    pass
        if tag == 0x41:  # 'A' dense array
            length = self._read_varint()
            out: list = []
            self.ids.append(out)
            n = 0
            while n < length:
                if b[self.i] == 0x2D:  # '-' hole
                    self.i += 1
                    out.append(None)
                    n += 1
                    continue
                out.append(self.value())
                n += 1
            # 尾随命名属性（少见）：读到 '$'。props 按原语义丢弃（不影响对话提取），
            # 原处为恒假死代码（`out.append(props) if False else None`），已清理
            props: dict = {}
            while b[self.i] != 0x24:  # '$'
                k = self.value()
                props[k] = self.value()
            self.i += 1
            self._read_varint()  # expected num props
            self._read_varint()  # expected length
            return out
        if tag == 0x44:  # 'D' date
            return {"__date__": self._double()}
        if tag == 0x42 or tag == 0x43:  # 'B'/'C' arraybuffer
            n = self._read_varint()
            self.i += n
            return {"__bytes_len__": n}
        if tag == 0x56:  # 'V' view
            self.i += 1  # subtag
            self._read_varint()
            self._read_varint()
            return {"__view__": True}
        if tag == 0x5A or tag == 0x7A:  # 'Z'/'z' bigint(-object)
            bf = self._u32le()
            n = (bf >> 1) & 0x3FFFFFFF
            sign = bf & 1
            nbytes = (n + 7) // 8 if n else 0
            raw = b[i : i + nbytes]
            self.i = i + nbytes
            v = int.from_bytes(raw, "little") if nbytes else 0
            return -v if sign else v
        if tag == 0x72:  # 'r' error
            while b[self.i] != 0x2E:  # '.'
                self.i += 1
                self.value()
            self.i += 1
            return {"__error__": True}
        if tag == 0x27:  # '\'' set
            items = []
            while b[self.i] != 0x2C:  # ','
                items.append(self.value())
            self.i += 1
            self._read_varint()
            return items
        if tag == 0x3B:  # ';' map
            m = {}
            while b[self.i] != 0x3A:  # ':'
                k = self.value()
                m[k] = self.value()
            self.i += 1
            self._read_varint()
            return m
        if tag == 0x5C:  # '\\' host object
            return {"__host__": True}
        raise ValueError(f"unknown v8 tag 0x{tag:02x} at {i - 1}")


def _strip_wrapper(v: bytes) -> bytes:
    """[ver][FF 15 FE][12×00][FF 0F][V8...] → V8 段（取前 32 字节内最后一个 FF 0F）。"""
    pos = -1
    limit = min(len(v), 32) - 1
    for k in range(limit):
        if v[k] == 0xFF and v[k + 1] == 0x0F:
            pos = k
    if pos < 0:
        return v
    return v[pos + 2 :]


def v8_deserialize(v: bytes):
    return _V8(_strip_wrapper(v)).value()


# ─────────────────────── IndexedDB 结构化视图 ───────────────────────

# 全局前缀（EncodeEmpty 4×00）后的 type byte
_GLOBAL_SCHEMA_VERSION = 0
_GLOBAL_SCOPES_PREFIX = 50
_GLOBAL_RECOVERY_BLOB_JOURNAL = 3
_GLOBAL_ACTIVE_BLOB_JOURNAL = 4
_DB_NAME_TYPE = 201

# prefix(db) 后的 type byte
_OS_META_TYPE = 50
_OS_DATA_INDEX_ID = 1
_OS_EXISTS_INDEX_ID = 2
_OS_BLOB_INDEX_ID = 3


def load_db_structured(db_dir: Path):
    """返回 {db_id: {os_id: {'name': str|None, 'data': {key_repr: value}, 'seq': {…}}}}。"""
    raw = read_leveldb_dir(db_dir)
    store_names: dict[tuple[int, int], str] = {}
    db_names: dict[int, str] = {}
    data: dict[int, dict[int, dict[bytes, tuple[int, bytes]]]] = {}

    for key, (seq, val) in raw.items():
        if not key:
            continue
        try:
            db, os_, idx, rest = decode_prefix(key)
        except (IndexError, ValueError):
            continue
        if db == 0:
            # 全局：0x32 开头是 scopes 恢复日志（真记录已同步写入，跳过）
            if rest and rest[0] in (_GLOBAL_SCOPES_PREFIX,):
                continue
            if os_ == 0 and idx == 0 and rest[:1] == bytes([_DB_NAME_TYPE]):
                try:
                    # DatabaseNameKey: origin(utf16be sentinel 串) + 0x00 + dbname
                    # 简化：取最后一个 0x00 之后按 utf16be 长度前缀解
                    tail = rest[1:]
                    pos = tail.rfind(b"\x00\x00")
                    name = tail[pos + 2 :].decode("utf-16-be", errors="replace") if pos >= 0 else ""
                    db_names[seq] = name
                except Exception:
                    pass
            continue
        data.setdefault(db, {}).setdefault(os_, {})
        if key[1] != db:  # 防御
            continue
        entry = data[db].setdefault(os_, {})
        if os_ == 0 and idx == 0 and rest[:1] == bytes([_OS_META_TYPE]):
            # ObjectStoreMetaDataKey: [50][varint os_id][meta_type]；value 为元数据
            continue
        if idx == _OS_DATA_INDEX_ID:
            try:
                k = decode_idb_key(rest)
            except Exception:
                k = rest.hex()
            entry.setdefault("data", {})[repr(k)] = (seq, val)
        elif idx == _OS_EXISTS_INDEX_ID:
            # 原 `repr(k if (k := None) else rest.hex())` 为恒取 else 分支的死代码写法，等价清理
            entry.setdefault("exists", {})[rest.hex()] = seq
    return {"db_names": db_names, "stores": data}


def dump_db_summary(db_dir: Path) -> str:
    """人类可读摘要：每个 db/os 的名称、记录数、样本。"""
    s = load_db_structured(db_dir)
    lines = [f"# {db_dir.name}"]
    for db_id in sorted(s["stores"]):
        for os_id in sorted(s["stores"][db_id]):
            st = s["stores"][db_id][os_id]
            recs = st.get("data", {})
            sizes = [len(v) for _, v in recs.values() if v]
            lines.append(
                f"db={db_id} os={os_id}: {len(recs)} 条数据记录, "
                f"总 {sum(sizes)} B, 平均 {sum(sizes) // len(sizes) if sizes else 0} B"
            )
    return "\n".join(lines)


# ─────────────────────── 豆包对话提取 ───────────────────────


def find_doubao_idb_dirs() -> list[Path]:
    """本机豆包 User Data 下的全部 IndexedDB leveldb 目录。"""
    base = Path(os.environ.get("LOCALAPPDATA", "")) / "Doubao" / "User Data"
    out = []
    if base.is_dir():
        for idb in base.glob("*/IndexedDB/*indexeddb.leveldb"):
            out.append(idb)
    return out


def extract_conversations(db_dir: Path) -> list[dict]:
    """扫描库内 {uid, data:{projects:[{projectId,name,conversations:[…]}]}} 记录。"""
    s = load_db_structured(db_dir)
    convs = []
    for db_id, stores in s["stores"].items():
        for os_id, st in stores.items():
            for _k, (seq, val) in st.get("data", {}).items():
                if not val:
                    continue
                try:
                    obj = v8_deserialize(val)
                except Exception:
                    continue
                if not isinstance(obj, dict) or "data" not in obj or "uid" not in obj:
                    continue
                inner = obj.get("data") or {}
                projects = inner.get("projects") if isinstance(inner, dict) else None
                if not isinstance(projects, list):
                    continue
                for p in projects:
                    if not isinstance(p, dict):
                        continue
                    conv_list = p.get("conversations")
                    if not isinstance(conv_list, list):
                        continue
                    for c in conv_list:
                        if isinstance(c, dict):
                            convs.append(
                                {
                                    "uid": obj.get("uid"),
                                    "project_id": p.get("projectId"),
                                    "project_name": p.get("name"),
                                    **c,
                                }
                            )
    # seq 降序去重（同一会话多份时取最新）
    convs.sort(key=lambda c: -c.get("update_time", 0) if isinstance(c.get("update_time"), (int, float)) else 0)
    return convs


def extract_messages(db_dir: Path) -> list[dict]:
    """扫描疑似消息记录（含 role/sender + content 的对象）。格式侦察用。"""
    s = load_db_structured(db_dir)
    msgs = []
    for db_id, stores in s["stores"].items():
        for os_id, st in stores.items():
            for _k, (seq, val) in st.get("data", {}).items():
                if not val or len(val) < 100:
                    continue
                try:
                    obj = v8_deserialize(val)
                except Exception:
                    continue
                _walk_messages(obj, msgs)
    return msgs


def _walk_messages(node, out: list, depth: int = 0):
    if depth > 8:
        return
    if isinstance(node, dict):
        keys = set(node.keys())
        if keys & {"role", "sender"} and keys & {"content", "text", "message"}:
            out.append(node)
        for v in node.values():
            _walk_messages(v, out, depth + 1)
    elif isinstance(node, list):
        for v in node:
            _walk_messages(v, out, depth + 1)


# ─────────────────────── 官方 API 客户端（D2 导出） ───────────────────────
# 端点（2026-09 从代理抓包实锤，需 cookie: sessionid/sid_tt/sid_guard + ttwid，头 agw-js-conv: str）：
#   POST https://www.doubao.com/im/chain/recent_conv?…   cmd=3200  会话列表（含 name/update_time）
#   POST https://www.doubao.com/im/chain/single?…        cmd=3100  单会话消息（含 content_block）
# 消息正文：content_block[].content.text_block.text（block_type 10000）；兜底 brief / tts_content。

UA = ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
      "(KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 SamanthaDoubao/2.27.12")
_RECENT_QS = ("version_code=20800&language=zh&device_platform=web&doubao_device_platform=desktop"
              "&aid=582478&real_aid=582478&pkg_type=release_version&device_id=199439841787403"
              "&pc_version=2.27.12&doubao_pc_version=2.27.12&region=CN&sys_region=CN&samantha_web=1"
              "&web_platform=desktop&use-olympus-account=1&runtime=web&runtime_version=3.35.4"
              "&client_platform=pc_client&chromium_version=147.0.7727.149&channel=win"
              # 设备指纹参数：缺 web_id / tea_uuid / fp 会被网关判 712010702 系统内部异常（实测对照确认）
              "&web_id=7681679574816589346&tea_uuid=199439841787403&fp=verify_199439841787403"
              "&web_tab_id=4db30b53-185f-4e73-99aa-6d29921381fc")
_SINGLE_QS = _RECENT_QS.replace("doubao_device_platform=desktop", "doubao_device_platform=web") \
                       .replace("aid=582478", "aid=497858").replace("real_aid=582478", "real_aid=497858") \
                       .replace("web_platform=desktop", "web_platform=web")


def _api_post(url: str, cookie: str, body: dict) -> dict:
    import urllib.request

    req = urllib.request.Request(
        url,
        data=json.dumps(body, separators=(",", ":")).encode("utf-8"),
        headers={
            "content-type": "application/json; encoding=utf-8",
            "cookie": cookie,
            "agw-js-conv": "str",
            "user-agent": UA,
            "accept": "application/json, text/plain, */*",
            "referer": "https://www.doubao.com/",
        },
        method="POST",
    )
    # 出站直连（不走系统代理）：与探活/额度巡检同约定
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(req, timeout=25) as resp:
        return json.loads(resp.read().decode("utf-8"))


def fetch_recent_convs(cookie: str, limit: int = 100) -> list[dict]:
    """拉取会话列表（按 conv_version 游标翻页）。
    注意：conv_version 首次请求必须为 int 0（字符串 "0" 会触发 712010702 系统内部异常，
    实测对照确认）；翻页时透传服务端返回的 next_conv_version 原始类型。"""
    import uuid

    convs: list[dict] = []
    cursor: object = 0  # int；翻页后为服务端返回的 next_conv_version（str）
    for _ in range(10):  # 硬上限防死循环
        body = {
            "cmd": 3200,
            "uplink_body": {"pull_recent_conv_chain_uplink_body": {
                "limit": min(limit, 50), "message_count_per_conv": 0, "api_version": 1,
                "conv_version": cursor, "direction": 3,
                "option": {"not_need_message": True, "need_complete_conversation": True,
                            "need_coco_bot": True, "need_pc_pin_chain": True, "pc_pin_query_type": 0,
                            "exclude_archive": True, "only_archive": False}}},
            "sequence_id": str(uuid.uuid4()), "channel": 2, "version": "1",
        }
        j = _api_post(f"https://www.doubao.com/im/chain/recent_conv?{_RECENT_QS}", cookie, body)
        if j.get("status_code") != 0:
            if cursor == 0:
                raise RuntimeError(f"recent_conv 失败: {j.get('status_code')} {j.get('status_desc', '')}")
            # 翻页跳失败：豆包客户端自身从不翻页（实测日志全部 conv_version=0），
            # 非首跳可能不被网关支持——降级为警告，返回已拉到的会话
            print(f"[warn] 会话列表翻页失败（已拉 {len(convs)} 个）: {j.get('status_code')} {j.get('status_desc', '')}", file=sys.stderr)
            break
        chain = j.get("downlink_body", {}).get("pull_recent_conv_chain_downlink_body", {})
        for cell in chain.get("cells", []):
            c = cell.get("conversation") or {}
            if c.get("conversation_id"):
                convs.append(c)
        if not chain.get("has_more"):
            break
        nxt = chain.get("next_conv_version") or ""
        if not nxt or nxt == cursor:
            break
        cursor = nxt
    return convs


def fetch_messages(cookie: str, conv_id: str, max_pages: int = 10) -> list[dict]:
    """拉取单会话消息（从最新向前翻页，最多 max_pages × 20 条）。"""
    import uuid

    msgs: list[dict] = []
    anchor = 9007199254740991  # 2^53-1 = 从最新开始
    for _ in range(max_pages):
        body = {
            "cmd": 3100,
            "uplink_body": {"pull_singe_chain_uplink_body": {
                "conversation_id": conv_id, "anchor_index": anchor, "conversation_type": 3,
                "direction": 1, "limit": 20, "ext": {}, "filter": {"index_list": []},
                "evaluate_ab_params": "", "evaluate_common_params": ""}},
            "sequence_id": str(uuid.uuid4()), "channel": 2, "version": "1",
        }
        j = _api_post(f"https://www.doubao.com/im/chain/single?{_SINGLE_QS}", cookie, body)
        if j.get("status_code") != 0:
            raise RuntimeError(f"chain/single 失败: {j.get('status_code')} {j.get('status_desc', '')}")
        page = j.get("downlink_body", {}).get("pull_singe_chain_downlink_body", {}).get("messages", [])
        if not page:
            break
        msgs = page + msgs  # 页内新→旧，向前拼接
        # index_in_conv 为字符串形式的大整数序列号，转 int 参与比较与回传
        try:
            oldest = min(int(m.get("index_in_conv") or 0) for m in page)
        except ValueError:
            break
        if oldest <= 0:
            break  # 已到会话开头
        anchor = oldest
    return msgs


def message_text(m: dict) -> str:
    """从消息记录提取正文：content_block text 优先，brief / tts_content 兜底。"""
    parts: list[str] = []
    for blk in m.get("content_block") or []:
        try:
            t = blk["content"]["text_block"]["text"]
            if t:
                parts.append(t)
        except (KeyError, TypeError):
            continue
    if parts:
        return "\n\n".join(parts)
    return m.get("content") or m.get("tts_content") or m.get("brief") or ""


def _load_account(data_dir: Path, uid: str) -> dict:
    pool_path = data_dir / "data" / "doubao_accounts.json"
    pool = json.loads(pool_path.read_text(encoding="utf-8")) if pool_path.exists() else {}
    for acc in pool.get("accounts", []):
        if acc.get("user_id") == uid:
            return acc
    raise SystemExit(f"账号 {uid} 不在账号池中（需先保存登录态/录入凭证）")


def _cookie_enc(v: str) -> str:
    """豆包 IM 网关要求 ttwid / sid_guard 以 URL 编码形式出现（| → %7C 等）；
    传原始 | / , / : 形式会报 712010702 系统内部异常（实测二分确认）。
    账号池统一存原始（可读）形式以便 sid_guard 到期解析；此处发送前编码。
    已含 % 的值视为已编码原样透传，避免双重编码。"""
    if "%" in v:
        return v
    import urllib.parse

    return urllib.parse.quote(v, safe="")


def _build_cookie(acc: dict) -> str:
    sid = acc.get("session_id") or ""
    if not sid:
        raise SystemExit(f"账号 {acc.get('user_id')} 未录入 sessionid（编辑账号 → 会话凭证，或开启代理自动抓取）")
    parts = [f"sessionid={sid}", f"sessionid_ss={sid}", f"sid_tt={sid}"]
    if acc.get("sid_guard"):
        parts.append(f"sid_guard={_cookie_enc(acc['sid_guard'])}")
    if acc.get("ttwid"):
        parts.append(f"ttwid={_cookie_enc(acc['ttwid'])}")
    else:
        print("[warn] 账号无 ttwid，对话 API 可能拒绝（登录校验不合法）；开启代理后访问豆包可自动抓取", file=sys.stderr)
    return "; ".join(parts)


def export_account(data_dir: Path, uid: str, limit_convs: int = 50, max_pages: int = 10) -> dict:
    """导出指定账号的对话记录 → data/exports/doubao_chats_<uid>_<ts>.md / .json"""
    acc = _load_account(data_dir, uid)
    cookie = _build_cookie(acc)
    convs = fetch_recent_convs(cookie, limit=limit_convs)[:limit_convs]
    out_convs = []
    for i, c in enumerate(convs):
        cid = c["conversation_id"]
        try:
            msgs = fetch_messages(cookie, cid, max_pages=max_pages)
        except RuntimeError as e:
            print(f"[warn] 会话 {cid} 拉取失败: {e}", file=sys.stderr)
            msgs = []
        slim = []
        for m in msgs:
            slim.append({
                "user_type": m.get("user_type"),
                "role": "user" if m.get("user_type") == 1 else "assistant",
                "section_name": m.get("section_name") or "",
                "create_time": m.get("create_time"),
                "text": message_text(m),
            })
        out_convs.append({
            "conversation_id": cid,
            "name": c.get("name") or f"会话 {cid}",
            "update_time": c.get("update_time"),
            "create_time": c.get("create_time"),
            "message_count": len(slim),
            "truncated": len(msgs) >= max_pages * 20,
            "messages": slim,
        })
        print(f"[{i + 1}/{len(convs)}] {out_convs[-1]['name']}（{len(slim)} 条）", file=sys.stderr)

    ts = time.strftime("%Y%m%d-%H%M%S")
    export_dir = data_dir / "data" / "exports"
    export_dir.mkdir(parents=True, exist_ok=True)
    json_path = export_dir / f"doubao_chats_{uid}_{ts}.json"
    md_path = export_dir / f"doubao_chats_{uid}_{ts}.md"
    payload = {"account": uid, "exported_at": time.strftime("%Y-%m-%d %H:%M:%S"),
               "conversation_count": len(out_convs), "conversations": out_convs}
    json_path.write_text(json.dumps(payload, ensure_ascii=False, indent=2), encoding="utf-8")
    md_path.write_text(to_markdown(payload), encoding="utf-8")
    return {
        "ok": True, "conversations": len(out_convs),
        "messages": sum(c["message_count"] for c in out_convs),
        "md_path": str(md_path), "json_path": str(json_path),
    }


def to_markdown(payload: dict) -> str:
    lines = [
        f"# 豆包对话记录导出 — 账号 {payload['account']}",
        "",
        f"> 导出时间：{payload['exported_at']} · 共 {payload['conversation_count']} 个会话",
        "",
    ]
    for c in payload["conversations"]:
        t = ""
        if c.get("update_time"):
            try:
                t = time.strftime("%Y-%m-%d %H:%M", time.localtime(int(c["update_time"])))
            except (ValueError, TypeError, OverflowError):
                pass
        trunc = "（消息较多，仅导出最近部分）" if c.get("truncated") else ""
        lines += [f"## {c['name']}", "", f"*更新：{t} · {c['message_count']} 条消息 {trunc}*", ""]
        cur_section = None
        for m in c["messages"]:
            if m.get("section_name") and m["section_name"] != cur_section:
                cur_section = m["section_name"]
                lines += [f"### {cur_section}", ""]
            who = "🧑 用户" if m["role"] == "user" else "🤖 豆包"
            ts = ""
            if m.get("create_time"):
                try:
                    ts = time.strftime("%H:%M", time.localtime(int(m["create_time"])))
                except (ValueError, TypeError, OverflowError):
                    pass
            lines += [f"**{who}** `{ts}`", "", m.get("text") or "（无文本内容）", ""]
        lines += ["---", ""]
    return "\n".join(lines)


# ─────────────────── 登录 Cookie 检测（快照/活动 profile 通用）───────────────────


def _list_chromium_profiles(base: Path) -> list:
    """列出 User Data 根（或快照槽）下的 Chromium Profile 目录（Default + Profile N）。
    豆包客户端自带多账号隔离（saman.account_isolation_config），登录会话可能位于
    任意 Profile —— 全部硬编码 Default 会导致快照漏抓/恢复落空/检测误判（实测根因）。"""
    if not base.is_dir():
        return []
    out = []
    try:
        for p in sorted(base.iterdir()):
            if p.is_dir() and (p.name == "Default" or p.name.startswith("Profile ")):
                out.append(p)
    except OSError:
        return []
    return out


def _read_active_profile_name(base: Path):
    """读 Local State → profile.last_used（客户端当前活跃 Profile 目录名）。"""
    ls = base / "Local State"
    if not ls.is_file():
        return None
    try:
        data = json.loads(ls.read_text(encoding="utf-8"))
        name = str(data.get("profile", {}).get("last_used") or "").strip()
        return name or None
    except Exception as e:  # noqa: BLE001
        print(f"[check-login] Local State 读取失败（忽略）: {e}", file=sys.stderr)
        return None


def _read_profile_session(prof: Path) -> dict:
    """读取单个 Chromium Profile 的 doubao.com 会话 cookie 概况。
    客户端运行中 Cookies 被独占锁（shutil.copy 报 Errno 13）：按读取失败处理，
    由调用方决定兜底语义。"""
    entry = {"doubao_cookies": 0, "has_session": False, "sessionid_remaining_sec": None, "error": None}
    net = prof / "Network"
    cookies_file = net / "Cookies"
    if not cookies_file.is_file():
        # 旧布局兜底：Cookies 直接在 Profile 目录下
        cookies_file = prof / "Cookies"
        if not cookies_file.is_file():
            entry["error"] = "no cookies file"
            return entry
    src_dir = net if (net / "Cookies").is_file() else prof
    tmp = Path(tempfile.mkdtemp(prefix="aw_cookie_check_"))
    try:
        # Cookies 及其 -journal/-wal 一并复制，避免读到未恢复的事务状态
        for f in src_dir.glob("Cookies*"):
            shutil.copy(f, tmp / f.name)
        con = sqlite3.connect(str(tmp / "Cookies"))
        try:
            cur = con.cursor()
            n = cur.execute("SELECT count(*) FROM cookies WHERE host_key LIKE '%doubao.com'").fetchone()[0]
            sess = cur.execute(
                "SELECT count(*) FROM cookies WHERE host_key LIKE '%doubao.com' AND name IN ('sessionid','sid_guard')"
            ).fetchone()[0]
            # sessionid/sid_guard 最小剩余有效期（秒）。expires_utc = 1601-01-01 起的微秒数。
            remaining = None
            for (exp,) in cur.execute(
                "SELECT expires_utc FROM cookies WHERE host_key LIKE '%doubao.com' "
                "AND name IN ('sessionid','sid_guard')"
            ):
                if not exp:  # 会话级 cookie，无持久有效期
                    remaining = 0
                    break
                remain_sec = exp / 1_000_000 - time.time() - 11644473600  # expires_utc 是 1601 起；换算回 Unix 时间差
                if remaining is None or remain_sec < remaining:
                    remaining = remain_sec
            entry.update(
                doubao_cookies=int(n),
                has_session=sess > 0,
                sessionid_remaining_sec=(int(remaining) if remaining is not None else None),
            )
        finally:
            con.close()
    except Exception as e:  # noqa: BLE001
        entry["error"] = str(e)[:120]
        print(f"[check-login] {prof.name} Cookies 读取失败: {e}", file=sys.stderr)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    return entry


def check_login_cookie(profile_dir: str) -> dict:
    """检测某目录（User Data 根 / 快照槽，多 Profile 布局）是否持有登录会话。

    Chromium Cookies 库的 cookie **名**为明文（值才加密），故可直接判定 sessionid 是否存在：
    - 解决「把未登录/已失效状态存进账号槽」这一反复污染快照的根因（实测 908 槽被未登录态
      反复覆盖：恢复后未登录 → 下次切换又把未登录态备份回该槽 → 死循环）。
    - uid 检测链会被 localStorage 残留/抓包过期数据骗过，但"有没有 sessionid cookie"不会说谎。

    多 Profile 语义（豆包自带账号隔离，活跃 Profile 随客户端切换漂移）：
    - 逐 Profile 读取概况；**优先取 Local State → profile.last_used（活跃 Profile）**的
      会话状态——客户端启动打开的就是它，它未登录 = 用户看到未登录；
    - 无 Local State / last_used 缺失（旧快照）时回退聚合语义：
      has_session = 任一 Profile 有会话，remaining = 有会话 Profile 中的最小剩余。

    返回 {"ok": bool, "doubao_cookies": int, "has_session": bool,
    "sessionid_remaining_sec": int|null, "active_profile": str|null,
    "profiles": [{name, doubao_cookies, has_session, sessionid_remaining_sec, error}]}。

    sessionid_remaining_sec 语义（与此前单 Profile 版本一致）：
    - 正常登录会话 sessionid 有效期 30 天（sid_guard 360 天）；
    - **退出登录后客户端会残留 6 小时有效期的匿名(游客) sessionid**——仅凭"有没有
      sessionid"无法区分真登录与游客态，必须看剩余时长（<12h 判为游客/临时会话，
      Rust 侧据此拒绝保存）。expires_utc=0 表示会话级 cookie（关浏览器即失效），视为 0 剩余。"""
    result: dict = {
        "ok": False,
        "doubao_cookies": 0,
        "has_session": False,
        "sessionid_remaining_sec": None,
        "active_profile": None,
        "profiles": [],
    }
    base = Path(profile_dir)
    profiles = _list_chromium_profiles(base)
    if not profiles:
        print("[check-login] 未找到任何 Profile 目录（Default / Profile N）", file=sys.stderr)
        return result

    per: dict = {}
    for prof in profiles:
        per[prof.name] = _read_profile_session(prof)

    active = _read_active_profile_name(base)
    if active not in per:
        active = None

    result["ok"] = any(e["doubao_cookies"] > 0 or e["error"] is None for e in per.values())
    result["doubao_cookies"] = sum(e["doubao_cookies"] for e in per.values())
    result["active_profile"] = active
    result["profiles"] = [dict(name=name, **e) for name, e in per.items()]

    if active is not None:
        # 活跃 Profile 语义：客户端启动打开的就是它
        e = per[active]
        result["has_session"] = e["has_session"]
        result["sessionid_remaining_sec"] = e["sessionid_remaining_sec"]
        if e["error"]:
            print(f"[check-login] 活跃 Profile {active} 读取失败: {e['error']}", file=sys.stderr)
    else:
        # 旧快照无 Local State：聚合语义
        cands = [e for e in per.values() if e["has_session"]]
        result["has_session"] = bool(cands)
        if cands:
            rems = [e["sessionid_remaining_sec"] for e in cands]
            result["sessionid_remaining_sec"] = (
                None if any(r is None for r in rems) else int(min(r for r in rems))
            )
    return result


# ─────────────────────── CLI ───────────────────────


def detect_uid_from_local_storage() -> dict:
    """读豆包客户端 Local Storage leveldb 的 client_device_info.userId（客户端每次启动自写，
    **不依赖代理**）——修复无代理时重新登录后识别不到新账号的问题（Local State 的 saman.user_id
    同 profile 重登不更新、抓包文件无代理不更新，均会返回旧账号）。

    与代理抓包文件（multi_sids 解析的 uid + captured_at）按时间戳比新鲜度取新者：
    代理开着时抓包随流量秒级更新、通常更新；代理没开时以客户端本地记录为准。
    返回 {"user_id": str|None, "source": "local_storage"|"captured"|None}（stdout 单行 JSON）。"""
    result: dict = {"user_id": None, "source": None, "launch_ts_ms": 0}
    ls_uid, ls_ts = None, 0.0
    ud = Path(os.environ.get("LOCALAPPDATA", "")) / "Doubao" / "User Data"
    # 多 Profile：豆包自带账号隔离（saman.account_isolation_config），登录会话/client_device_info
    # 写在**活跃 Profile**的 leveldb 里（活跃 = Local State → profile.last_used）。硬编码 Default
    # 会在客户端切到 Profile N 后读到旧账号（实测根因）。策略：活跃 Profile 优先；
    # 无 last_used / 读取失败时，取全部 Profile 中 currentLaunchTime 最新的记录。
    active_name = None
    ls_state = ud / "Local State"
    if ls_state.is_file():
        try:
            active_name = str(json.loads(ls_state.read_text(encoding="utf-8"))
                              .get("profile", {}).get("last_used") or "").strip() or None
        except Exception as e:  # noqa: BLE001
            print(f"[detect-uid] Local State 读取失败（忽略）: {e}", file=sys.stderr)

    ldb_dirs = []
    if ud.is_dir():
        try:
            names = [p.name for p in sorted(ud.iterdir())
                     if p.is_dir() and (p.name == "Default" or p.name.startswith("Profile "))]
        except OSError:
            names = []
        # 活跃 Profile 排最前，其余按名称序兜底
        if active_name in names:
            names.remove(active_name)
            names.insert(0, active_name)
        elif active_name:
            names.insert(0, active_name)  # last_used 指向的目录可能尚不存在，跳过即可
        ldb_dirs = [ud / n / "Local Storage" / "leveldb" for n in names]
    best = None  # (launch_ms, seq, uid) —— 活跃 Profile 优先，其次 launch_ms/seq 最新
    for idx, ldb in enumerate(ldb_dirs):
        if not ldb.is_dir():
            continue
        try:
            data = read_leveldb_dir(ldb)
        except Exception as e:  # 客户端运行中文件锁等：探测失败不应中断调用方
            print(f"[detect-uid] leveldb 读取失败（{ldb.parents[2].name}）: {e}", file=sys.stderr)
            continue
        for k, (seq, v) in data.items():
            if b"client_device_info" not in k:
                continue
            s = v[1:].decode("utf-8", "replace") if v[:1] == b"\x01" else v.decode("utf-8", "replace")
            i = s.find("{")
            if i < 0:
                continue
            try:
                j = json.loads(s[i:])
            except Exception:
                continue
            uid = str(j.get("userId") or "").strip()
            lm = j.get("currentLaunchTime")
            if uid.isdigit() and len(uid) >= 10:
                cand = (int(lm) if isinstance(lm, (int, float)) else 0, seq, uid)
                # idx=0 即活跃 Profile（或无 Local State 时的 Default）：命中即用；
                # 否则与现有 best 比 launch_ms 取新。
                # （原内层 `if best is None or idx == 0 or cand[0] > best[0]:` 恒被外层
                # 条件蕴含——外层真时内层必真——属冗余死分支，等价合并，行为不变）
                if best is None or (idx == 0 and best[2] != uid and active_name) or cand[0] > best[0]:
                    best = cand
        if best and idx == 0 and active_name:
            break  # 活跃 Profile 已给出 uid，无需再扫
    if best:
        ls_uid, ls_ts = best[2], best[0] / 1000.0
    # 抓包文件新鲜度
    cap_uid, cap_ts = None, 0.0
    data_dir = Path(os.environ.get("AIWORKDATA_DIR") or os.environ.get("TRAEDATA_DIR")
                    or os.path.dirname(os.path.abspath(__file__)))
    cap_file = data_dir / "data" / "doubao_captured_credentials.json"
    if cap_file.is_file():
        try:
            c = json.loads(cap_file.read_text(encoding="utf-8"))
            u = str(c.get("uid") or "").strip()
            ts = str(c.get("captured_at") or "").strip()
            if u.isdigit() and ts:
                cap_uid = u
                cap_ts = time.mktime(time.strptime(ts, "%Y-%m-%d %H:%M:%S"))
        except Exception as e:
            print(f"[detect-uid] 抓包文件读取失败: {e}", file=sys.stderr)
    if cap_uid and cap_ts > ls_ts:
        result.update(user_id=cap_uid, source="captured", launch_ts_ms=int(best[0]) if best else 0)
    elif ls_uid:
        result.update(user_id=ls_uid, source="local_storage", launch_ts_ms=int(best[0]) if best else 0)
    elif cap_uid:
        result.update(user_id=cap_uid, source="captured")
    else:
        # Local State info_cache 兜底（最低优先级）：部分客户端版本不再把 client_device_info
        # 写进 Local Storage leveldb（实测 2026-09-09），此时取活跃 Profile 的 saman.user_id。
        # 同 profile 重登不更新，仅作最后兜底（守卫链还有 Cookies 会话校验兜底防误判）。
        try:
            info = (json.loads((ud / "Local State").read_text(encoding="utf-8"))
                    .get("profile", {}).get("info_cache", {}) or {})
            cand = None
            if active_name and active_name in info:
                cand = str(info[active_name].get("saman", {}).get("user_id") or "").strip()
            if not (cand and cand.isdigit() and len(cand) >= 10):
                for e in info.values():
                    u = str(e.get("saman", {}).get("user_id") or "").strip()
                    if u.isdigit() and len(u) >= 10:
                        cand = u
                        break
            if cand:
                result.update(user_id=cand, source="local_state")
        except Exception as e:
            print(f"[detect-uid] Local State info_cache 兜底读取失败: {e}", file=sys.stderr)
    return result


def main() -> int:
    ap = argparse.ArgumentParser(description="豆包对话 IndexedDB 解析与导出")
    ap.add_argument("--dir", help="指定 leveldb 目录（默认自动扫描）")
    ap.add_argument("--scan", action="store_true", help="打印库结构摘要")
    ap.add_argument("--dump-conv", action="store_true", help="打印会话列表")
    ap.add_argument("--dump-msg", action="store_true", help="打印消息样本（侦察）")
    ap.add_argument("--export", action="store_true", help="走 API 导出对话记录（md/json）")
    ap.add_argument("--detect-uid", action="store_true", help="识别豆包客户端当前登录 uid（不依赖代理）")
    ap.add_argument("--check-login-cookie", metavar="DIR",
                    help="检测 profile 目录（User Data 布局）是否持有登录会话 Cookie（sessionid/sid_guard）")
    ap.add_argument("--uid", help="导出的账号 user_id（账号池内）")
    ap.add_argument("--limit-convs", type=int, default=50, help="最多导出的会话数（默认 50）")
    ap.add_argument("--max-pages", type=int, default=10, help="每会话最多翻页数 ×20 条（默认 10 页）")
    args = ap.parse_args()

    if args.detect_uid:
        print(json.dumps(detect_uid_from_local_storage(), ensure_ascii=False))
        return 0

    if args.check_login_cookie:
        print(json.dumps(check_login_cookie(args.check_login_cookie), ensure_ascii=False))
        return 0

    if args.export:
        uid = args.uid or ""
        if not uid:
            print("缺少 --uid", file=sys.stderr)
            return 1
        data_dir = Path(os.environ.get("AIWORKDATA_DIR") or os.environ.get("TRAEDATA_DIR")
                    or os.path.dirname(os.path.abspath(__file__)))
        summary = export_account(data_dir, uid, limit_convs=args.limit_convs, max_pages=args.max_pages)
        print(json.dumps(summary, ensure_ascii=False))
        return 0

    dirs = [Path(args.dir)] if args.dir else find_doubao_idb_dirs()
    if not dirs:
        print("未找到豆包 IndexedDB 目录", file=sys.stderr)
        return 1
    for d in dirs:
        if args.scan:
            print(dump_db_summary(d))
        if args.dump_conv:
            for c in extract_conversations(d)[:30]:
                print(json.dumps(c, ensure_ascii=False)[:400])
        if args.dump_msg:
            for m in extract_messages(d)[:10]:
                print(json.dumps(m, ensure_ascii=False, default=str)[:600])
    return 0


if __name__ == "__main__":
    sys.exit(main())
