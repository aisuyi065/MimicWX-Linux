#!/usr/bin/env python3
"""
GDB Python 脚本: 自动提取微信 WCDB 加密密钥

用法 (由 start.sh 自动调用):
  gdb -batch -p <wechat_pid> -x /usr/local/bin/extract_key.py

原理:
  1. 附加到运行中的微信进程
  2. 在 setCipherKey (WCDB wrapper) 偏移处设置断点
  3. 用户扫码登录后, 微信调用 setCipherKey 打开数据库
  4. 断点触发时从 $rsi 寄存器读取 Data 结构体中的 32 字节密钥
  5. 保存密钥到文件后 detach
"""

import gdb
import re
import sys
import os

# 输出重定向到 stderr (避免被 gdb -batch 吞掉)
sys.stdout = sys.stderr

# =====================================================================
# 配置
# =====================================================================

# WeChat 4.1.0.16 的 setCipherKey 偏移
SETCIPHERKEY_OFFSET = 0x6586C90

# 密钥保存路径
KEY_FILE = "/tmp/wechat_key.txt"

# 微信二进制路径 (容器内)
WECHAT_BINARY = "/opt/wechat/wechat"

# =====================================================================
# GDB 初始化
# =====================================================================

gdb.execute("set pagination off")
gdb.execute("set confirm off")

print("[extract_key] 🔑 GDB 密钥提取脚本启动")

# =====================================================================
# 获取微信基地址
# =====================================================================

def get_wechat_base():
    """从 /proc/pid/maps 或 info proc mapping 获取微信基地址"""
    try:
        output = gdb.execute("info proc mapping", to_string=True)
        for line in output.splitlines():
            line = line.strip()
            if WECHAT_BINARY in line and "r-x" in line:
                # 找到代码段 (可执行)
                addr = line.split()[0]
                return int(addr, 16)
            elif WECHAT_BINARY in line:
                addr = line.split()[0]
                return int(addr, 16)
    except Exception as e:
        print(f"[extract_key] ❌ info proc mapping 失败: {e}")

    # 回退: 从 /proc/pid/maps 读取
    try:
        pid = gdb.selected_inferior().pid
        with open(f"/proc/{pid}/maps", "r") as f:
            for line in f:
                if WECHAT_BINARY in line and "r-xp" in line:
                    addr = line.split("-")[0]
                    return int(addr, 16)
                elif WECHAT_BINARY in line:
                    addr = line.split("-")[0]
                    return int(addr, 16)
    except Exception as e:
        print(f"[extract_key] ❌ /proc/maps 读取失败: {e}")

    return None


base = get_wechat_base()
if base is None:
    print("[extract_key] ❌ 无法获取微信基地址, 退出")
    gdb.execute("detach")
    gdb.execute("quit")

bp_addr = base + SETCIPHERKEY_OFFSET
print(f"[extract_key] 📍 微信基地址: {hex(base)}")
print(f"[extract_key] 📍 断点地址: {hex(bp_addr)}")


# =====================================================================
# 断点类: 捕获 setCipherKey 调用
# =====================================================================

class SetCipherKeyBreakpoint(gdb.Breakpoint):
    """在 setCipherKey 上设置断点, 捕获加密密钥"""

    def __init__(self, addr):
        super().__init__(f"*{hex(addr)}", gdb.BP_BREAKPOINT)
        self._hits = 0
        self.captured_key = None

    def stop(self):
        """断点触发回调. 返回 False = 不停止, 继续运行"""
        self._hits += 1

        try:
            # 读取寄存器
            rsi = int(gdb.parse_and_eval("$rsi"))
            rdx = int(gdb.parse_and_eval("$rdx"))
            ecx = int(gdb.parse_and_eval("$ecx"))

            print(f"[extract_key] 🔑 [{self._hits}] HIT! page_size={rdx}, cipher_version={ecx}")

            # Data 结构体布局: [vtable/type(8), void* data(8), size_t size(8)]
            raw_ptr = gdb.execute(f"x/1gx {rsi + 8}", to_string=True)
            ptr = int(raw_ptr.split(":")[1].strip().split()[0], 16)

            raw_sz = gdb.execute(f"x/1gx {rsi + 16}", to_string=True)
            sz = int(raw_sz.split(":")[1].strip().split()[0], 16)

            if 0 < sz <= 256 and ptr > 0x1000:
                # 读取密钥字节
                # 重要: 逐行解析 GDB x/Nbx 输出, 只取冒号后面的数据
                # 避免把地址中的 0xNN 也当成数据
                raw_bytes = gdb.execute(f"x/{sz}bx {ptr}", to_string=True)
                hex_values = []
                for line in raw_bytes.strip().splitlines():
                    # 每行格式: "0x76d6f0003ba0:\t0x39\t0xa1\t..."
                    # 取冒号后面的部分
                    if ":" in line:
                        data_part = line.split(":", 1)[1]
                    else:
                        data_part = line
                    hex_values.extend(re.findall(r"0x([0-9a-fA-F]{2})", data_part))

                key_hex = "".join(hex_values)
                print(f"[extract_key] 🔑 [{self._hits}] 密钥({sz}字节): {key_hex}")

                # 只保存第一次捕获的密钥
                if self.captured_key is None:
                    self.captured_key = key_hex
                    try:
                        with open(KEY_FILE, "w") as f:
                            f.write(key_hex)
                        print(f"[extract_key] ✅ 密钥已保存到 {KEY_FILE}")
                    except Exception as e:
                        print(f"[extract_key] ❌ 保存密钥失败: {e}")

                    # 首次捕获后, 删除断点并计划 detach
                    gdb.post_event(self._cleanup)
            else:
                print(f"[extract_key] ⚠️ [{self._hits}] 异常: ptr={hex(ptr)} size={sz}")

        except Exception as e:
            print(f"[extract_key] ❌ 提取失败: {e}")

        return False  # 不停止, 让微信继续运行

    def _cleanup(self):
        """清理断点并 detach"""
        try:
            print("[extract_key] 🔓 密钥已获取, 正在 detach...")
            gdb.execute("delete breakpoints")
            gdb.execute("detach")
            print("[extract_key] ✅ GDB 已 detach, 微信正常运行")
            gdb.execute("quit")
        except Exception as e:
            print(f"[extract_key] ⚠️ detach 过程异常: {e}")
            try:
                gdb.execute("quit")
            except:
                pass


# =====================================================================
# 设置断点并等待
# =====================================================================

bp = SetCipherKeyBreakpoint(bp_addr)
print(f"[extract_key] ⏳ 断点已设置, 等待用户扫码登录...")
print(f"[extract_key] 📱 请通过 noVNC (http://localhost:6080/vnc.html) 扫码登录微信")

# 继续执行 — GDB 将在此阻塞直到断点触发或进程退出
gdb.execute("continue")
