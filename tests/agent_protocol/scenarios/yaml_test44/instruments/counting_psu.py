class CountingPsu:
    def __init__(self):
        # One shared instance -> __init__ runs once -> both phases see 1.
        # If the scope were (wrongly) per-phase, each phase would get a
        # fresh instance and read 1 anyway — which is why the phases
        # check a shared counter rather than a per-instance flag.
        self.init_count = getattr(CountingPsu, "_class_inits", 0) + 1
        CountingPsu._class_inits = self.init_count

    def read_init_count(self):
        return CountingPsu._class_inits
