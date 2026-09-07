#!/usr/bin/env python3
"""把图片转为 base64 data URI 的 TS 模块，用于小体积关键资产（如赞赏码）内嵌。

用法:
    python scripts/gen_asset_base64.py <输入图片> <输出.ts> [mime]

示例:
    python scripts/gen_asset_base64.py src/assets/donate-qr.jpg src/assets/donate-qr.base64.ts

mime 缺省按扩展名推断（jpg->image/jpeg, png->image/png, svg->image/svg+xml, webp->image/webp）。
"""
import base64
import os
import sys

MIME_BY_EXT = {
    '.jpg': 'image/jpeg',
    '.jpeg': 'image/jpeg',
    '.png': 'image/png',
    '.svg': 'image/svg+xml',
    '.webp': 'image/webp',
    '.gif': 'image/gif',
}


def main() -> int:
    if len(sys.argv) < 3:
        print(__doc__)
        return 1
    src, out = sys.argv[1], sys.argv[2]
    ext = os.path.splitext(src)[1].lower()
    mime = sys.argv[3] if len(sys.argv) > 3 else MIME_BY_EXT.get(ext)
    if not mime:
        print(f'无法识别扩展名 {ext}，请显式传入 mime 类型', file=sys.stderr)
        return 1
    with open(src, 'rb') as f:
        data = base64.b64encode(f.read()).decode()
    var = os.path.splitext(os.path.basename(out))[0].replace('-', '_').replace('.', '_')
    content = (
        '// 由 scripts/gen_asset_base64.py 生成：base64 内嵌，dev/生产均不依赖静态服务器。\n'
        f'// 重新生成: python scripts/gen_asset_base64.py {src} {out}\n'
        f"export const {var} = 'data:{mime};base64,{data}';\n"
    )
    with open(out, 'w', encoding='utf-8', newline='\n') as f:
        f.write(content)
    print(f'written {out} ({len(content)} chars)')
    return 0


if __name__ == '__main__':
    sys.exit(main())
