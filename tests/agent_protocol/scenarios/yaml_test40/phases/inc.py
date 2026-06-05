def inc(log, shared_counter, per_slot_counter):
    shared_counter.inc()
    per_slot_counter.inc()
    log.info(f"shared={shared_counter.value()} per_slot={per_slot_counter.value()}")
