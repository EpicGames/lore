# SPDX-FileCopyrightText: 2026 Epic Games, Inc.
# SPDX-License-Identifier: MIT
import hashlib
import http.client
import json
import logging

import pytest
from lore import Lore

logger = logging.getLogger(__name__)


@pytest.mark.smoke
def test_presign_vend_redeem_and_reject_head(
    new_lore_repo, request, lore_local_server_config, lore_main_server_ports
):
    repo: Lore = new_lore_repo("presign")
    file_name = "presigned-payload.bin"
    contents = b"presigned integration payload\n" * 4096

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
    port = lore_main_server_ports["http"]

    vend_path = f"/v1/repository/{repository_id}/content/{address}/presign"
    vend_body = json.dumps(
        {"ttl_seconds": 3600, "content_type": "application/octet-stream"}
    )
    connection = http.client.HTTPConnection(hostname, port)
    connection.request(
        "POST",
        vend_path,
        body=vend_body,
        headers={"content-type": "application/json"},
    )
    vend_response = connection.getresponse()
    vend_response_body = vend_response.read()
    assert vend_response.status == 200
    redemption_path = json.loads(vend_response_body)["url_suffix"]

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
        "presign flow: repository=%s address=%s bytes=%d sha256=%s vend=%d head=%d get=%d",
        repository_id,
        address,
        len(contents),
        hashlib.sha256(redeemed_contents).hexdigest(),
        vend_response.status,
        head_response.status,
        redeem_response.status,
    )
    logger.info("local server log: %s", lore_local_server_config[0] / "server.log")
