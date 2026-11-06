# The GitHub runner app's one image (docs/spec-runner-app.md §2 in LehrNAS): both
# services run it, and the environment picks the role (start.sh).
#
# Built from the upstream release tarball, not GitHub's image, which adds a
# docker client and Kubernetes hooks this app doesn't want. `broker-bin/<arch>`
# is the static broker, cross-built before the image (see the release command);
# the base and the tarballs are pinned, so one version builds one thing.
FROM docker.io/library/ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852

ARG TARGETARCH
ARG RUNNER_VERSION=2.338.2
ARG RUNNER_SHA256_amd64=baebf204307bac6193480852732f0aba2ae97b6c5597c244f4d5558390147c40
ARG RUNNER_SHA256_arm64=670adb2f268fe21dcbb58b3db0e0aa9d5b3bddfe2c651481336e561ff86de945

# tini: a real PID 1, or a job's orphans stay as zombies.
# curl: the broker's way to GitHub (no TLS stack in the binary).
# util-linux: ipcrm, for the clean between jobs (on the base already; named here).
# git, libicu, libssl, tar, zstd and the rest: what the runner and jobs commonly need.
# The libnss3 line onward: Chromium's shared libraries, for Playwright jobs.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      tini curl ca-certificates util-linux git tar gzip zstd unzip xz-utils jq \
      libicu74 libssl3t64 libkrb5-3 zlib1g liblttng-ust1t64 \
      libnss3 libnspr4 libatk1.0-0t64 libatk-bridge2.0-0t64 libcups2t64 libdrm2 \
      libxkbcommon0 libxcomposite1 libxdamage1 libxfixes3 libxrandr2 libgbm1 \
      libpango-1.0-0 libcairo2 libasound2t64 fonts-liberation \
 && rm -rf /var/lib/apt/lists/*

# USER is a number, and it has no /etc/subuid entry: a nested podman would then
# fail on newuidmap (the spike), and Chromium's sandbox wants a non-root user.
RUN groupadd --gid 1001 runner \
 && useradd --uid 1001 --gid 1001 --home-dir /tmp/runner/home --no-create-home --shell /bin/bash runner \
 && sed -i '/^runner:/d' /etc/subuid /etc/subgid \
 && mkdir -p /config /cache && chown 1001:1001 /config /cache

# The tarball is verified before it's unpacked; the state the runner writes is
# linked into tmpfs, because it crashes at once on a read-only directory.
RUN set -eu; \
    case "$TARGETARCH" in amd64) arch=x64; want=$RUNNER_SHA256_amd64 ;; arm64) arch=arm64; want=$RUNNER_SHA256_arm64 ;; *) exit 1 ;; esac; \
    curl -fsSL -o /tmp/runner.tar.gz "https://github.com/actions/runner/releases/download/v${RUNNER_VERSION}/actions-runner-linux-${arch}-${RUNNER_VERSION}.tar.gz"; \
    echo "$want  /tmp/runner.tar.gz" | sha256sum -c -; \
    mkdir -p /opt/runner; \
    tar -xzf /tmp/runner.tar.gz -C /opt/runner; \
    rm -f /tmp/runner.tar.gz; \
    printf '%s\n' "$RUNNER_VERSION" > /opt/runner/VERSION; \
    chown -R 0:0 /opt/runner; \
    for f in _diag .runner .credentials .credentials_rsaparams .env .path; do \
      ln -s "/tmp/runner/$f" "/opt/runner/$f"; \
    done

COPY broker-bin/${TARGETARCH}/gha-runner-broker /usr/local/bin/gha-runner-broker
COPY start.sh runner-start.sh /usr/local/lib/gha-runner/
COPY docker-stub /usr/local/bin/docker
RUN chmod 0755 /usr/local/bin/gha-runner-broker /usr/local/bin/docker /usr/local/lib/gha-runner/*.sh

USER 1001:1001
WORKDIR /opt/runner
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/lib/gha-runner/start.sh"]
