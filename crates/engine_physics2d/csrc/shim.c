// Flat C API over Box2D v3 for engine_physics2d. Ids cross the boundary as integers and
// definitions as plain arguments, so the Rust side never mirrors Box2D's structs (their
// layout changes between versions). Written for this engine; not part of Box2D.

#include "box2d/box2d.h"

#include <stdint.h>
#include <string.h>

static uint32_t world_to_u32(b2WorldId id)
{
	uint32_t v;
	memcpy(&v, &id, sizeof v);
	return v;
}

static b2WorldId world_from_u32(uint32_t v)
{
	b2WorldId id;
	memcpy(&id, &v, sizeof id);
	return id;
}

static uint64_t body_to_u64(b2BodyId id)
{
	uint64_t v;
	memcpy(&v, &id, sizeof v);
	return v;
}

static b2BodyId body_from_u64(uint64_t v)
{
	b2BodyId id;
	memcpy(&id, &v, sizeof id);
	return id;
}

uint32_t e2d_world_create(float gx, float gy)
{
	_Static_assert(sizeof(b2WorldId) == 4, "b2WorldId is 4 bytes");
	_Static_assert(sizeof(b2BodyId) == 8, "b2BodyId is 8 bytes");
	b2WorldDef def = b2DefaultWorldDef();
	def.gravity = (b2Vec2){gx, gy};
	// Single-threaded: Box2D runs its solver stages on the calling thread without a task
	// system. Multithreading goes through our job system later (ADR 0028).
	def.workerCount = 1;
	return world_to_u32(b2CreateWorld(&def));
}

void e2d_world_destroy(uint32_t world) { b2DestroyWorld(world_from_u32(world)); }

void e2d_world_step(uint32_t world, float dt, int substeps)
{
	b2World_Step(world_from_u32(world), dt, substeps);
}

int e2d_world_awake_bodies(uint32_t world) { return b2World_GetAwakeBodyCount(world_from_u32(world)); }

// kind: 0 static, 1 kinematic, 2 dynamic.
uint64_t e2d_body_create(uint32_t world, int kind, float x, float y, float angle)
{
	b2BodyDef def = b2DefaultBodyDef();
	def.type = kind == 0 ? b2_staticBody : kind == 1 ? b2_kinematicBody : b2_dynamicBody;
	def.position = (b2Vec2){x, y};
	def.rotation = b2MakeRot(angle);
	return body_to_u64(b2CreateBody(world_from_u32(world), &def));
}

void e2d_body_destroy(uint64_t body) { b2DestroyBody(body_from_u64(body)); }

int e2d_body_valid(uint64_t body) { return b2Body_IsValid(body_from_u64(body)) ? 1 : 0; }

static b2ShapeDef shape_def(float density, float friction, float restitution)
{
	b2ShapeDef def = b2DefaultShapeDef();
	def.density = density;
	def.material.friction = friction;
	def.material.restitution = restitution;
	def.enableContactEvents = true;
	return def;
}

void e2d_body_add_box(uint64_t body, float hx, float hy, float density, float friction, float restitution)
{
	b2ShapeDef def = shape_def(density, friction, restitution);
	b2Polygon box = b2MakeBox(hx, hy);
	b2CreatePolygonShape(body_from_u64(body), &def, &box);
}

void e2d_body_add_circle(uint64_t body, float radius, float density, float friction, float restitution)
{
	b2ShapeDef def = shape_def(density, friction, restitution);
	b2Circle circle = {{0.0f, 0.0f}, radius};
	b2CreateCircleShape(body_from_u64(body), &def, &circle);
}

// out: x, y, angle.
void e2d_body_transform(uint64_t body, float* out)
{
	b2Transform t = b2Body_GetTransform(body_from_u64(body));
	out[0] = t.p.x;
	out[1] = t.p.y;
	out[2] = b2Rot_GetAngle(t.q);
}

void e2d_body_set_transform(uint64_t body, float x, float y, float angle)
{
	b2Body_SetTransform(body_from_u64(body), (b2Vec2){x, y}, b2MakeRot(angle));
}

// out: vx, vy, angular.
void e2d_body_velocity(uint64_t body, float* out)
{
	b2BodyId id = body_from_u64(body);
	b2Vec2 v = b2Body_GetLinearVelocity(id);
	out[0] = v.x;
	out[1] = v.y;
	out[2] = b2Body_GetAngularVelocity(id);
}

void e2d_body_set_velocity(uint64_t body, float vx, float vy, float angular)
{
	b2BodyId id = body_from_u64(body);
	b2Body_SetLinearVelocity(id, (b2Vec2){vx, vy});
	b2Body_SetAngularVelocity(id, angular);
}

// Writes up to `capacity` begin-touch pairs of body ids (a, b) into `out` (2 per event)
// and returns the number of events of the last step.
int e2d_world_begin_contacts(uint32_t world, uint64_t* out, int capacity)
{
	b2ContactEvents events = b2World_GetContactEvents(world_from_u32(world));
	int n = events.beginCount < capacity ? events.beginCount : capacity;
	for (int i = 0; i < n; ++i)
	{
		out[2 * i] = body_to_u64(b2Shape_GetBody(events.beginEvents[i].shapeIdA));
		out[2 * i + 1] = body_to_u64(b2Shape_GetBody(events.beginEvents[i].shapeIdB));
	}
	return events.beginCount;
}
