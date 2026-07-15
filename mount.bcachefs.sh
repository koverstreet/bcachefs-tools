#!/bin/bash

join_by()
{
    local IFS="$1"
    shift
    echo "$*"
}

args=$(getopt -u -o 'sfnvo:t:N:' -n 'mount.bcachefs' -- "$@")
if [ $? -ne 0 ]; then
    echo 'Terminating...' >&2
    exit 1
fi

read -r -a argv <<< "$args"

for i in ${!argv[@]}; do
    [[ ${argv[$i]} == '--' ]] && break
done

i=$((i+1))

if [[ $((i + 2)) < ${#argv[@]} ]]; then
    echo "Insufficient arguments"
    exit 1
fi

UUID=${argv[$i]}

if [[ ${UUID//-/} =~ ^[[:xdigit:]]{32}$ ]]; then
    PARTS=()

    for part in $(tail -n +3 /proc/partitions|awk '{print $4}'); do
	uuid_line=$(timeout 3 bcachefs show-super /dev/$part 2>/dev/null | head -n1)

	if [[ $uuid_line =~ $UUID ]]; then
	    PARTS+=(/dev/$part)
	fi
    done

    if [[ ${#PARTS[@]} == 0 ]]; then
	echo "uuid $UUID not found"
	exit 1
    fi

    argv[$i]=$(join_by : "${PARTS[@]}")
fi

exec mount -i -t bcachefs ${argv[@]}
