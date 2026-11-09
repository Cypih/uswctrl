#!/bin/sh
# Redacts secret-looking values in key=value fixture lines before they are committed.

pattern='password|passwd|authkey|\.key=|\.key\.[0-9]+\.value|psk|secret'

while IFS= read -r line || [ -n "$line" ]; do
	case "$line" in
	*=*)
		key="${line%%=*}"
		if printf '%s' "${key}=" | grep -Eq "$pattern"; then
			printf '%s=REDACTED\n' "$key"
		else
			printf '%s\n' "$line"
		fi
		;;
	*)
		printf '%s\n' "$line"
		;;
	esac
done
