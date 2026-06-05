import json


def report(attach, log):
    attach.data(json.dumps({"summary": "ok"}).encode(), "summary.json")
    log.info("report attached")
