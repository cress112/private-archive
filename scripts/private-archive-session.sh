#!/bin/bash
# Shared launch boundary for CLI, GUI, and the macOS bundle.
# Credentials remain in process memory/environment, never shell code or argv.
if [[ "${BASH_SOURCE[0]}" != "$0" ]]; then
    return 0
fi
set +x
set +v

if [[ $# -lt 1 ]]; then
    printf '%s\n' 'private-archive: session launcher requires an executable' >&2
    exit 2
fi
archive_binary=$1
shift
archive_profile=${PRIVATE_ARCHIVE_AWS_PROFILE:-private-archive}
PATH="${PATH:-/usr/bin:/bin}:/usr/local/bin:/opt/homebrew/bin"
export PATH
archive_aws=$(type -P aws) || {
    printf '%s\n' 'private-archive: AWS CLI v2 is required by the authentication wrapper' >&2
    exit 127
}

unset AWS_ACCESS_KEY_ID AWS_SECRET_ACCESS_KEY AWS_SESSION_TOKEN AWS_SECURITY_TOKEN \
    AWS_CREDENTIAL_EXPIRATION AWS_ROLE_ARN AWS_ROLE_SESSION_NAME AWS_WEB_IDENTITY_TOKEN_FILE \
    AWS_CONTAINER_CREDENTIALS_RELATIVE_URI AWS_CONTAINER_CREDENTIALS_FULL_URI \
    AWS_CONTAINER_AUTHORIZATION_TOKEN AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE \
    AWS_PROFILE AWS_DEFAULT_PROFILE AWS_ENDPOINT_URL AWS_ENDPOINT_URL_S3 AWS_ENDPOINT_URL_STS
export AWS_EC2_METADATA_DISABLED=true AWS_CLI_AUTO_PROMPT=off AWS_PAGER=""

archive_region=${AWS_REGION:-${AWS_DEFAULT_REGION:-}}
if [[ -z "$archive_region" ]]; then
    archive_region=$("$archive_aws" configure get region --profile "$archive_profile") || archive_region=""
fi
if [[ -z "$archive_region" ]]; then
    printf '%s\n' 'private-archive: configure a region in the wrapper profile or AWS_REGION' >&2
    exit 1
fi
archive_credentials=$("$archive_aws" configure export-credentials --profile "$archive_profile" --format env-no-export) || {
    printf '%s\n' 'private-archive: could not resolve wrapper credentials; application was not started' >&2
    exit 1
}

archive_seen=" "
while IFS= read -r archive_line; do
    [[ -n "$archive_line" ]] || continue
    archive_key=${archive_line%%=*}
    archive_value=${archive_line#*=}
    case "$archive_key" in
        AWS_ACCESS_KEY_ID|AWS_SECRET_ACCESS_KEY|AWS_SESSION_TOKEN|AWS_CREDENTIAL_EXPIRATION) ;;
        *) printf '%s\n' 'private-archive: invalid credential export' >&2; exit 1 ;;
    esac
    if [[ "$archive_line" != *=* || -z "$archive_value" || "$archive_seen" == *" $archive_key "* ]]; then
        printf '%s\n' 'private-archive: incomplete or duplicate credential export' >&2
        exit 1
    fi
    export "$archive_key=$archive_value"
    archive_seen+="$archive_key "
done <<< "$archive_credentials"
if [[ -z "${AWS_ACCESS_KEY_ID:-}" || -z "${AWS_SECRET_ACCESS_KEY:-}" ]]; then
    printf '%s\n' 'private-archive: credential export did not contain an access key pair' >&2
    exit 1
fi
export AWS_REGION="$archive_region" AWS_DEFAULT_REGION="$archive_region"
unset PRIVATE_ARCHIVE_AWS_PROFILE archive_credentials archive_value archive_line
exec "$archive_binary" "$@"
