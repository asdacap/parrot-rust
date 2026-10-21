#!/bin/sh
# Immutable test executable: announces the wrong literal readiness token.
printf '%s' "$$" > "${PARROT_TEST_HELPER_PID_PATH:?}"
printf 'NOTREADY\n' >&2
exec sleep 30
