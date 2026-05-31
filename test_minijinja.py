with open("Cargo.toml", "r") as f:
    c = f.read()
if "minijinja" in c.split("[dependencies]")[1].split("[dev-dependencies]")[0]:
    print("yes")
else:
    print("no")
