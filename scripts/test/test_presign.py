# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import hashlib
import http.client
import json
import logging
import random

import pytest

from lore import Lore

logger = logging.getLogger(__name__)


def push_and_presign(repo: Lore, file_name, contents, vend_fields, request, ports):
    """Pushes `contents` as `file_name` and presigns it with `vend_fields`.

    Returns the open server connection, the redemption path, the repository id
    and the address.
    """
    with repo.open_file(file_name, "wb+") as output_file:
        output_file.write(contents)

    repo.stage(scan=True)
    repo.commit()
    repo.push(level="debug")

    file_info = repo.file_info(file_name)[0]
    status_output = repo.status()
    repository_id = status_output.split("Repository ", 1)[1].splitlines()[0]
    address = f"{file_info.hash}-{file_info.context}"
    hostname = request.config.getoption("--lore-server-hostname")

    connection = http.client.HTTPConnection(hostname, ports["http"])
    connection.request(
        "POST",
        f"/v1/repository/{repository_id}/content/{address}/presign",
        body=json.dumps({"ttl_seconds": 3600, **vend_fields}),
        headers={"content-type": "application/json"},
    )
    vend_response = connection.getresponse()
    vend_response_body = vend_response.read()
    assert vend_response.status == 200
    redemption_path = json.loads(vend_response_body)["url_suffix"]
    return connection, redemption_path, repository_id, address


@pytest.mark.smoke
def test_presign_vend_redeem_and_reject_head(
    new_lore_repo, request, lore_local_server_config, lore_main_server_ports
):
    contents = b"presigned integration payload\n" * 4096
    connection, redemption_path, repository_id, address = push_and_presign(
        new_lore_repo("presign"),
        "presigned-payload.bin",
        contents,
        {"content_type": "application/octet-stream"},
        request,
        lore_main_server_ports,
    )

    connection.request("HEAD", redemption_path)
    head_response = connection.getresponse()
    assert head_response.status == 405
    assert head_response.getheader("allow") == "GET"
    assert head_response.read() == b""

    for method in ("POST", "OPTIONS"):
        connection.request(method, redemption_path)
        response = connection.getresponse()
        assert response.status == 405
        assert response.getheader("allow") == "GET"
        assert response.read() == b""

    connection.request("GET", redemption_path)
    redeem_response = connection.getresponse()
    redeemed_contents = redeem_response.read()
    connection.close()

    assert redeem_response.status == 200
    assert redeem_response.getheader("content-type") == "application/octet-stream"
    assert redeemed_contents == contents

    logger.info(
        "presign flow: repository=%s address=%s bytes=%d sha256=%s head=%d get=%d",
        repository_id,
        address,
        len(contents),
        hashlib.sha256(redeemed_contents).hexdigest(),
        head_response.status,
        redeem_response.status,
    )
    logger.info("local server log: %s", lore_local_server_config[0] / "server.log")


@pytest.mark.smoke
def test_presign_redeem_serves_a_byte_range(
    new_lore_repo, request, lore_main_server_ports
):
    contents = random.Random(1234).randbytes(4 * 1024 * 1024)
    connection, redemption_path, repository_id, address = push_and_presign(
        new_lore_repo("presign-range"),
        "presigned-range.bin",
        contents,
        {},
        request,
        lore_main_server_ports,
    )

    start = 3 * 1024 * 1024 + 17
    end = start + 65535
    connection.request(
        "GET", redemption_path, headers={"range": f"bytes={start}-{end}"}
    )
    range_response = connection.getresponse()
    ranged_contents = range_response.read()
    connection.close()

    assert range_response.status == 206
    assert (
        range_response.getheader("content-range")
        == f"bytes {start}-{end}/{len(contents)}"
    )
    assert range_response.getheader("accept-ranges") == "bytes"
    assert ranged_contents == contents[start : end + 1]

    logger.info(
        "presign range: repository=%s address=%s range=%d-%d of %d bytes",
        repository_id,
        address,
        start,
        end,
        len(contents),
    )
