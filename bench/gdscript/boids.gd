# Boids in GDScript: the same step as crates/engine_script/benches/boids.rs (1,000 agents,
# every pair, cohesion and separation within radius 5, then integrate), for comparing
# Godot's scripting with Luau and Rust on the same machine (ADR 0032).
#
#   godot --headless --script bench/gdscript/boids.gd -- out=<file.md>
#
# The table is printed and, with `out=`, also written to a file (the windowed godot.exe
# may not deliver stdout when it is redirected).
# Positions start uniformly in a 100 m cube like the Rust/Luau bench (same density; the
# random sequence differs). Prints a Markdown table. Written for this engine's
# benchmarks; not part of Godot.
extends SceneTree

const N := 1000
const STEPS := 10

func make_data(seed_value: int) -> Array:
	var rng := RandomNumberGenerator.new()
	rng.seed = seed_value
	var pos := PackedVector3Array()
	var vel := PackedVector3Array()
	pos.resize(N)
	vel.resize(N)
	for i in N:
		pos[i] = Vector3(rng.randf() * 100.0, rng.randf() * 100.0, rng.randf() * 100.0)
		vel[i] = Vector3(rng.randf() - 0.5, rng.randf() - 0.5, rng.randf() - 0.5)
	return [pos, vel]

# Static types everywhere: Godot 4 emits typed instructions for these.
func step_typed(pos: PackedVector3Array, vel: PackedVector3Array) -> void:
	for i: int in N:
		var p: Vector3 = pos[i]
		var center := Vector3.ZERO
		var push := Vector3.ZERO
		var count := 0
		for j: int in N:
			if i == j:
				continue
			var d: Vector3 = pos[j] - p
			var d2: float = d.dot(d)
			if d2 < 25.0:
				center += pos[j]
				push -= d / (d2 + 0.01)
				count += 1
		if count > 0:
			vel[i] += (center / count - p) * 0.01 + push * 0.05
	for i: int in N:
		vel[i] *= 0.99
		pos[i] += vel[i] * 0.1

# The same without type annotations, on plain Arrays.
func step_untyped(pos, vel):
	for i in N:
		var p = pos[i]
		var center = Vector3.ZERO
		var push = Vector3.ZERO
		var count = 0
		for j in N:
			if i == j:
				continue
			var d = pos[j] - p
			var d2 = d.dot(d)
			if d2 < 25.0:
				center += pos[j]
				push -= d / (d2 + 0.01)
				count += 1
		if count > 0:
			vel[i] += (center / count - p) * 0.01 + push * 0.05
	for i in N:
		vel[i] *= 0.99
		pos[i] += vel[i] * 0.1

func median(v: Array) -> float:
	v.sort()
	return v[v.size() / 2]

func _init() -> void:
	var data := make_data(12345)
	var pos: PackedVector3Array = data[0]
	var vel: PackedVector3Array = data[1]
	var typed := []
	for s in STEPS:
		var t := Time.get_ticks_usec()
		step_typed(pos, vel)
		typed.append((Time.get_ticks_usec() - t) / 1000.0)

	data = make_data(12345)
	var apos: Array = Array(data[0])
	var avel: Array = Array(data[1])
	var untyped := []
	for s in STEPS:
		var t := Time.get_ticks_usec()
		step_untyped(apos, avel)
		untyped.append((Time.get_ticks_usec() - t) / 1000.0)

	var info := Engine.get_version_info()
	var lines := PackedStringArray()
	lines.append("### GDScript boids (Godot %s, %d agents, all pairs; ms, median of %d)" % [info.string, N, STEPS])
	lines.append("")
	lines.append("| variant | ms |")
	lines.append("|---|---:|")
	lines.append("| GDScript, static types, PackedVector3Array | %.2f |" % median(typed))
	lines.append("| GDScript, untyped, Array of Vector3 | %.2f |" % median(untyped))
	lines.append("")
	var text := "\n".join(lines)
	print(text)
	for arg in OS.get_cmdline_user_args():
		if arg.begins_with("out="):
			var f := FileAccess.open(arg.substr(4), FileAccess.WRITE)
			if f:
				f.store_string(text + "\n")
				f.close()
	quit()
