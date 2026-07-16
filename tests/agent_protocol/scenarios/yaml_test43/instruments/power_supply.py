class PowerSupply:
    def __init__(self):
        # takes no kwargs; the config's bogus_key does not match, so
        # instantiation raises TypeError and the run errors out.
        self.voltage = 0.0
