# syntax=docker/dockerfile:1
FROM quay.io/pypa/manylinux_2_28_x86_64 AS build
ENV PATH="/root/.cargo/bin:/opt/python/cp311-cp311/bin:${PATH}"
WORKDIR /src
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup.sh \
    && sh /tmp/rustup.sh -y --profile minimal --default-toolchain 1.89.0 \
    && rm /tmp/rustup.sh
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY bindings/c ./bindings/c
COPY bindings/python ./bindings/python
COPY bindings/build_wheel.py ./bindings/build_wheel.py
COPY tests ./tests
RUN python bindings/build_wheel.py --output /unrepaired \
    && auditwheel repair --plat manylinux_2_28_x86_64 --wheel-dir /wheels /unrepaired/*.whl \
    && auditwheel show /wheels/*.whl \
    && python tests/embedded_wheel.py /wheels --platform-tag manylinux_2_28_x86_64

FROM scratch AS artifacts
COPY --from=build /wheels/ /
