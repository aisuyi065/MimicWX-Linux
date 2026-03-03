# MimicWX-Linux Docker 环境
# 多阶段构建: builder 编译 Rust → runtime 部署二进制
#
# 构建: docker build -t mimicwx .
# 加速: 默认使用国内镜像 (阿里云 APT + rsproxy Cargo)
#       海外构建可传 --build-arg USE_MIRROR=0 关闭

ARG USE_MIRROR=1

# ================================================================
# Stage 1: Builder (编译 Rust 二进制)
# ================================================================
FROM ubuntu:22.04 AS builder

ARG USE_MIRROR
ENV DEBIAN_FRONTEND=noninteractive

# APT 源: 国内用阿里云加速
RUN if [ "$USE_MIRROR" = "1" ]; then \
    sed -i 's|http://archive.ubuntu.com|http://mirrors.aliyun.com|g' /etc/apt/sources.list && \
    sed -i 's|http://security.ubuntu.com|http://mirrors.aliyun.com|g' /etc/apt/sources.list; \
    fi

# 编译依赖
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config curl ca-certificates \
    libdbus-1-dev libatspi2.0-dev libglib2.0-dev \
    && rm -rf /var/lib/apt/lists/*

# Rust 工具链 (国内用 rsproxy 加速)
RUN if [ "$USE_MIRROR" = "1" ]; then \
    export RUSTUP_DIST_SERVER=https://rsproxy.cn && \
    export RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup && \
    curl -sSf https://rsproxy.cn/rustup-init.sh | sh -s -- -y --default-toolchain stable --profile minimal; \
    else \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal; \
    fi
ENV PATH="/root/.cargo/bin:$PATH"

# Cargo 镜像 (国内用 rsproxy sparse index)
RUN if [ "$USE_MIRROR" = "1" ]; then \
    mkdir -p /root/.cargo && \
    printf '[source.crates-io]\nreplace-with = "ustc"\n[source.ustc]\nregistry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"\n' \
    > /root/.cargo/config.toml; \
    fi

WORKDIR /build

# 先复制 Cargo 文件利用依赖缓存
COPY Cargo.toml Cargo.lock* ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release && \
    rm -rf src target/release/mimicwx target/release/deps/mimicwx-*

# 复制实际源码并编译
COPY src/ src/
RUN cargo build --release

# ================================================================
# Stage 2: Runtime (运行环境)
# ================================================================
FROM ubuntu:22.04

ARG USE_MIRROR
ENV DEBIAN_FRONTEND=noninteractive
ENV LANG=zh_CN.UTF-8
ENV LANGUAGE=zh_CN:zh
ENV LC_ALL=zh_CN.UTF-8
ENV TZ=Asia/Shanghai

# APT 源: 国内用阿里云加速
RUN if [ "$USE_MIRROR" = "1" ]; then \
    sed -i 's|http://archive.ubuntu.com|http://mirrors.aliyun.com|g' /etc/apt/sources.list && \
    sed -i 's|http://security.ubuntu.com|http://mirrors.aliyun.com|g' /etc/apt/sources.list; \
    fi

# 基础包 + 桌面环境 + VNC + 微信依赖 (合并为一次 apt-get, 减少层数)
RUN apt-get update && apt-get install -y \
    ca-certificates \
    locales fonts-wqy-microhei fonts-wqy-zenhei fonts-noto-cjk \
    xfce4 xfce4-terminal dbus-x11 \
    tigervnc-standalone-server tigervnc-common \
    novnc websockify \
    at-spi2-core \
    xclip x11-utils \
    wget curl sudo procps net-tools gpg \
    gdb python3 \
    libcap2-bin libatomic1 \
    # 微信 Qt/xcb 运行时依赖
    libxkbcommon-x11-0 libxcb-icccm4 libxcb-image0 libxcb-keysyms1 \
    libxcb-render-util0 libxcb-xinerama0 libxcb-shape0 libxcb-cursor0 \
    libxcb-xkb1 libxcb-randr0 libnss3 libatk1.0-0 libatk-bridge2.0-0 \
    libcups2 libdrm2 libgbm1 libasound2 libpango-1.0-0 \
    libcairo2 libatspi2.0-0 libgtk-3-0 \
    && rm -rf /var/lib/apt/lists/*

# 中文 locale + 时区
RUN locale-gen zh_CN.UTF-8 && \
    ln -sf /usr/share/zoneinfo/Asia/Shanghai /etc/localtime && \
    echo "Asia/Shanghai" > /etc/timezone

# 安装微信 (官方 .deb)
RUN wget -q -O /tmp/wechat.deb \
    "https://dldir1v6.qq.com/weixin/Universal/Linux/WeChatLinux_x86_64.deb" && \
    dpkg -i /tmp/wechat.deb || apt-get install -f -y && \
    rm -f /tmp/wechat.deb && rm -rf /var/lib/apt/lists/*

# 创建用户
RUN useradd -m -s /bin/bash -G sudo wechat && \
    echo "wechat:wechat" | chpasswd && \
    echo "wechat ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

# 从 builder 复制编译好的二进制
COPY --from=builder /build/target/release/mimicwx /usr/local/bin/mimicwx
RUN chmod +x /usr/local/bin/mimicwx && \
    setcap cap_sys_admin+ep /usr/local/bin/mimicwx

# VNC 配置
USER wechat
WORKDIR /home/wechat

RUN mkdir -p ~/.vnc && \
    echo "mimicwx" | vncpasswd -f > ~/.vnc/passwd && \
    chmod 600 ~/.vnc/passwd

RUN printf '#!/bin/bash\nunset SESSION_MANAGER\nunset DBUS_SESSION_BUS_ADDRESS\nexport XKL_XMODMAP_DISABLE=1\nexec startxfce4\n' > ~/.vnc/xstartup && \
    chmod +x ~/.vnc/xstartup

# 启动脚本
USER root
COPY docker/dbus-mimicwx.conf /etc/dbus-1/session.d/mimicwx.conf
COPY docker/start.sh /usr/local/bin/start.sh
COPY docker/extract_key.py /usr/local/bin/extract_key.py
RUN sed -i 's/\r$//' /usr/local/bin/start.sh /usr/local/bin/extract_key.py && \
    chmod +x /usr/local/bin/start.sh /usr/local/bin/extract_key.py

EXPOSE 5901 6080 8899
CMD ["/usr/local/bin/start.sh"]
