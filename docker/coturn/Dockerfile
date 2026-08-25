# coturn, with the one thing the upstream image cannot do: follow a changing public
# address.
#
# This is a thin layer over the official image rather than a coturn build of our own.
# Everything that faces the network is upstream's, patched by upstream, on upstream's
# schedule; what is added is one POSIX shell script that decides which address coturn is
# told about. See deploy/coturn-dynamic-ip.md for the conditions this assumes.
#
# 4.17.2 is the current stable (2026-08-10), on the same Alpine 3.24 the server image
# uses, so one distribution's advisories cover both. Digest-pinned to the amd64 manifest.
FROM coturn/coturn:4.17.2-alpine3.24@sha256:ea86b1d11fb4e75960570684538cd438a05d60c09921f4dd6af4e840066e42bb

LABEL org.opencontainers.image.title="coturn for AnotherCrewLink" \
      org.opencontainers.image.description="coturn with dynamic external-ip discovery for residential lines" \
      org.opencontainers.image.source="https://github.com/greluc/AnotherCrewLink-Server" \
      org.opencontainers.image.base.name="docker.io/coturn/coturn:4.17.2-alpine3.24"

COPY --chmod=0555 entrypoint.sh /usr/local/bin/acl-coturn-entrypoint

# Upstream's image already runs as an unprivileged user; this does not change it.
ENTRYPOINT ["/usr/local/bin/acl-coturn-entrypoint"]
