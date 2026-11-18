#!/bin/sh
# Immutable test executable: deliberately never announces bridge readiness.
printf '%s' "$$" > "${PARROT_TEST_HELPER_PID_PATH:?}"
exec sleep 30
