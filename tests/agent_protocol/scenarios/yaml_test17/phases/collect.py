import json


def collect(attach, log):
    log.info("collecting")
    data = json.dumps({"hello": "world"}).encode()
    attach.data(data, "payload.json")
    log.info("attached")
