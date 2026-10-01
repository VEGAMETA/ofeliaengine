// Flat C API over Box2D v3 for engine_physics2d. Ids cross the boundary as integers and
// definitions as plain arguments, so the Rust side never mirrors Box2D's structs (their
// layout changes between versions). Written for this engine; not part of Box2D.

#include "box2d/box2d.h"

#include <float.h>
#include <math.h>
#include <stdbool.h>
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

// `layer` 0..63 is the shape's category bit (queries and movers filter by layer masks);
// the shape collides with every layer.
static b2ShapeDef shape_def(float density, float friction, float restitution, int layer)
{
	b2ShapeDef def = b2DefaultShapeDef();
	def.density = density;
	def.material.friction = friction;
	def.material.restitution = restitution;
	def.enableContactEvents = true;
	def.filter.categoryBits = (uint64_t)1 << (layer & 63);
	return def;
}

void e2d_body_add_box(uint64_t body, float hx, float hy, float density, float friction, float restitution, int layer)
{
	b2ShapeDef def = shape_def(density, friction, restitution, layer);
	b2Polygon box = b2MakeBox(hx, hy);
	b2CreatePolygonShape(body_from_u64(body), &def, &box);
}

void e2d_body_add_circle(uint64_t body, float radius, float density, float friction, float restitution, int layer)
{
	b2ShapeDef def = shape_def(density, friction, restitution, layer);
	b2Circle circle = {{0.0f, 0.0f}, radius};
	b2CreateCircleShape(body_from_u64(body), &def, &circle);
}

// A capsule along the body's y axis: the segment from (0, -half_length) to (0, half_length).
void e2d_body_add_capsule(uint64_t body, float half_length, float radius, float density, float friction, float restitution, int layer)
{
	b2ShapeDef def = shape_def(density, friction, restitution, layer);
	b2Capsule capsule = {{0.0f, -half_length}, {0.0f, half_length}, radius};
	b2CreateCapsuleShape(body_from_u64(body), &def, &capsule);
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

// Moves a kinematic body to reach the target at the end of the next step of `dt`.
void e2d_body_set_target(uint64_t body, float x, float y, float angle, float dt)
{
	b2Transform target = {{x, y}, b2MakeRot(angle)};
	b2Body_SetTargetTransform(body_from_u64(body), target, dt);
}

// ---------------------------------------------------------------------------------------
// Queries (ADR 0066). Hits are written as 5 floats (fraction, point x, y, normal x, y) and a
// body id each; the functions return the number of hits found (which may exceed the
// capacity: the caller grows its buffers and asks again).

// kind 0: circle (a = radius); 1: box (a, b = half extents); 2: capsule along y
// (a = half length of the segment, b = radius).
static b2ShapeProxy make_proxy(int kind, float a, float b, float x, float y, float angle)
{
	b2Vec2 position = {x, y};
	b2Rot rotation = b2MakeRot(angle);
	if (kind == 0)
		return b2MakeProxy(&position, 1, a);
	if (kind == 1)
	{
		b2Vec2 corners[4] = {{-a, -b}, {a, -b}, {a, b}, {-a, b}};
		return b2MakeOffsetProxy(corners, 4, 0.0f, position, rotation);
	}
	b2Vec2 ends[2] = {{0.0f, -a}, {0.0f, a}};
	return b2MakeOffsetProxy(ends, 2, b, position, rotation);
}

static b2QueryFilter query_filter(uint64_t layers)
{
	b2QueryFilter filter = {UINT64_MAX, layers};
	return filter;
}

typedef struct CastContext
{
	uint64_t ignore;
	int closest;
	float* hits;
	uint64_t* bodies;
	int capacity;
	int count;
} CastContext;

static float cast_callback(b2ShapeId shape, b2Vec2 point, b2Vec2 normal, float fraction, void* context)
{
	CastContext* c = (CastContext*)context;
	uint64_t body = body_to_u64(b2Shape_GetBody(shape));
	if (body == c->ignore || b2Shape_IsSensor(shape))
		return -1.0f; // skip this shape
	int slot = c->closest ? 0 : c->count;
	if (slot < c->capacity)
	{
		float* h = c->hits + 5 * slot;
		h[0] = fraction;
		h[1] = point.x;
		h[2] = point.y;
		h[3] = normal.x;
		h[4] = normal.y;
		c->bodies[slot] = body;
	}
	if (c->closest)
	{
		c->count = 1;
		return fraction; // only closer hits from now on
	}
	c->count += 1;
	return 1.0f; // every hit
}

int e2d_cast_ray(uint32_t world, float ox, float oy, float tx, float ty, uint64_t layers, uint64_t ignore, int closest, float* hits, uint64_t* bodies, int capacity)
{
	CastContext c = {ignore, closest, hits, bodies, capacity, 0};
	b2World_CastRay(world_from_u32(world), (b2Vec2){ox, oy}, (b2Vec2){tx, ty}, query_filter(layers), cast_callback, &c);
	return c.count;
}

int e2d_cast_shape(uint32_t world, int kind, float a, float b, float x, float y, float angle, float tx, float ty, uint64_t layers, uint64_t ignore, int closest, float* hits, uint64_t* bodies, int capacity)
{
	CastContext c = {ignore, closest, hits, bodies, capacity, 0};
	b2ShapeProxy proxy = make_proxy(kind, a, b, x, y, angle);
	b2World_CastShape(world_from_u32(world), &proxy, (b2Vec2){tx, ty}, query_filter(layers), cast_callback, &c);
	return c.count;
}

typedef struct OverlapContext
{
	uint64_t ignore;
	uint64_t* bodies;
	int capacity;
	int count;
} OverlapContext;

static bool overlap_callback(b2ShapeId shape, void* context)
{
	OverlapContext* c = (OverlapContext*)context;
	uint64_t body = body_to_u64(b2Shape_GetBody(shape));
	if (body == c->ignore || b2Shape_IsSensor(shape))
		return true;
	if (c->count < c->capacity)
		c->bodies[c->count] = body;
	c->count += 1;
	return true;
}

int e2d_overlap_shape(uint32_t world, int kind, float a, float b, float x, float y, float angle, uint64_t layers, uint64_t ignore, uint64_t* bodies, int capacity)
{
	OverlapContext c = {ignore, bodies, capacity, 0};
	b2ShapeProxy proxy = make_proxy(kind, a, b, x, y, angle);
	b2World_OverlapShape(world_from_u32(world), &proxy, query_filter(layers), overlap_callback, &c);
	return c.count;
}

// ---------------------------------------------------------------------------------------
// Character mover (ADR 0066), after the mover sample of Box2D 3.1
// (samples/sample_character.cpp, MIT): collide the capsule with the world to gather
// planes, solve them, cast the capsule along the solved translation, repeat; clip the
// velocity against the planes. Added: a slope limit (too steep upward-facing planes act
// as vertical walls), ground detection and snapping down to the ground.

#define E2D_MAX_PLANES 16

typedef struct MoverContext
{
	b2CollisionPlane planes[E2D_MAX_PLANES];
	int count;
	float cos_max_slope;
	// The most upward-facing walkable plane.
	float ground_ny;
	b2Vec2 ground_normal;
	b2Vec2 ground_point;
	uint64_t ground_body;
} MoverContext;

static bool plane_callback(b2ShapeId shape, const b2PlaneResult* result, void* context)
{
	MoverContext* m = (MoverContext*)context;
	if (b2Shape_IsSensor(shape))
		return true;
	b2Plane plane = result->plane;
	b2Vec2 n = plane.normal;
	if (n.y >= m->cos_max_slope)
	{
		if (n.y > m->ground_ny)
		{
			m->ground_ny = n.y;
			m->ground_normal = n;
			// Box2D 3.1.1 reports the point in the shape's body space.
			b2BodyId body = b2Shape_GetBody(shape);
			m->ground_point = b2Body_GetWorldPoint(body, result->point);
			m->ground_body = body_to_u64(body);
		}
	}
	else if (n.y > 0.0f && fabsf(n.x) > 1e-6f)
	{
		// Too steep to walk: a vertical wall with the same push along its normal.
		float nx = n.x > 0.0f ? 1.0f : -1.0f;
		plane.offset = plane.offset / fabsf(n.x);
		plane.normal = (b2Vec2){nx, 0.0f};
	}
	if (m->count < E2D_MAX_PLANES)
	{
		m->planes[m->count] = (b2CollisionPlane){plane, FLT_MAX, 0.0f, true};
		m->count += 1;
	}
	return true;
}

static void collide(uint32_t world, const b2Capsule* mover, b2QueryFilter filter, MoverContext* m)
{
	m->count = 0;
	m->ground_ny = -1.0f;
	m->ground_body = 0;
	b2World_CollideMover(world_from_u32(world), mover, filter, plane_callback, m);
}

static b2Capsule capsule_at(b2Vec2 p, float bottom, float top, float radius)
{
	b2Capsule c = {{p.x, p.y + bottom}, {p.x, p.y + top}, radius};
	return c;
}

// in: position x, y; velocity x, y; dt; radius; height; cos(max slope); snap distance;
//     was grounded (0/1).
// out: position x, y; velocity x, y; ground normal x, y; ground point x, y; ground
//      velocity x, y; grounded (0/1).
void e2d_mover_move(uint32_t world, const float* in, uint64_t layers, float* out, uint64_t* ground_body)
{
	b2Vec2 position = {in[0], in[1]};
	b2Vec2 velocity = {in[2], in[3]};
	float dt = in[4];
	float radius = in[5];
	float height = in[6];
	float snap = in[8];
	int was_grounded = in[9] != 0.0f;
	// The capsule stands on `position`.
	float bottom = radius;
	float top = height - radius > radius ? height - radius : radius + 0.001f;
	b2QueryFilter filter = query_filter(layers);
	MoverContext m;
	m.cos_max_slope = in[7];

	b2Vec2 target = b2MulAdd(position, dt, velocity);
	const float tolerance = 0.001f;
	for (int iteration = 0; iteration < 5; ++iteration)
	{
		b2Capsule mover = capsule_at(position, bottom, top, radius);
		collide(world, &mover, filter, &m);
		b2PlaneSolverResult result = b2SolvePlanes(b2Sub(target, position), m.planes, m.count);
		float fraction = b2World_CastMover(world_from_u32(world), &mover, result.translation, filter);
		b2Vec2 delta = b2MulSV(fraction, result.translation);
		position = b2Add(position, delta);
		if (b2LengthSquared(delta) < tolerance * tolerance)
			break;
	}
	velocity = b2ClipVector(velocity, m.planes, m.count);

	// Where it stands now.
	b2Capsule mover = capsule_at(position, bottom, top, radius);
	collide(world, &mover, filter, &m);
	if (m.ground_ny < 0.0f && was_grounded && snap > 0.0f && velocity.y <= 0.0f)
	{
		// Walked off a step or over the crest of a slope: pull down to the ground if it
		// is within reach.
		b2Vec2 down = {0.0f, -snap};
		float fraction = b2World_CastMover(world_from_u32(world), &mover, down, filter);
		if (fraction < 1.0f)
		{
			position = b2MulAdd(position, fraction, down);
			mover = capsule_at(position, bottom, top, radius);
			collide(world, &mover, filter, &m);
		}
	}
	int grounded = m.ground_ny >= 0.0f;
	b2Vec2 ground_velocity = {0.0f, 0.0f};
	if (grounded)
	{
		ground_velocity = b2Body_GetWorldPointVelocity(body_from_u64(m.ground_body), m.ground_point);
		// Standing: no velocity into the ground remains.
		velocity = b2ClipVector(velocity, m.planes, m.count);
	}
	out[0] = position.x;
	out[1] = position.y;
	out[2] = velocity.x;
	out[3] = velocity.y;
	out[4] = grounded ? m.ground_normal.x : 0.0f;
	out[5] = grounded ? m.ground_normal.y : 0.0f;
	out[6] = grounded ? m.ground_point.x : 0.0f;
	out[7] = grounded ? m.ground_point.y : 0.0f;
	out[8] = ground_velocity.x;
	out[9] = ground_velocity.y;
	out[10] = (float)grounded;
	*ground_body = grounded ? m.ground_body : 0;
}
