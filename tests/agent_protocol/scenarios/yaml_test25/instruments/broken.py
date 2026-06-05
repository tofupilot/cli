class Broken:
    def __init__(self):
        raise RuntimeError("cannot connect to instrument")
