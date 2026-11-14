"""Cosmos DB's data plane in depth: databases and containers, patch, the
change feed, throughput, and the SQL query language, case by case. Expected
query results follow Cosmos DB's documented semantics (undefined values,
`??`, string ordering, half-away-from-zero ROUND)."""

import time

import pytest
from azure.core import MatchConditions
from azure.cosmos import PartitionKey, exceptions

from test_cosmos import account, client  # noqa: F401  (fixtures)

PEOPLE = [
    {"id": "1", "pk": "p", "name": "Alice", "age": 30, "tags": ["a", "b"], "address": {"city": "Oslo"}, "score": -2.5, "active": True},
    {"id": "2", "pk": "p", "name": "bob", "age": 25, "tags": ["b"], "address": {"city": "Bergen"}, "nick": None, "score": 3.7, "active": False},
    {"id": "3", "pk": "p", "name": "Carol", "age": 35, "tags": [], "score": 10},
]


@pytest.fixture
def people(client):
    container = client.create_database_if_not_exists("querydb").create_container_if_not_exists("people", partition_key=PartitionKey(path="/pk"))
    for person in PEOPLE:
        container.upsert_item(person)
    return container


def values(container, query, **kwargs):
    return list(container.query_items(query, partition_key="p", **kwargs))


# (WHERE clause, ids of the matching people)
FILTERS = [
    ("IS_DEFINED(c.address)", ["1", "2"]),
    ("NOT IS_DEFINED(c.address)", ["3"]),
    ("IS_NULL(c.nick)", ["2"]),
    ("IS_STRING(c.name)", ["1", "2", "3"]),
    ("IS_NUMBER(c.score)", ["1", "2", "3"]),
    ("IS_BOOL(c.active)", ["1", "2"]),
    ("IS_ARRAY(c.tags)", ["1", "2", "3"]),
    ("IS_OBJECT(c.address)", ["1", "2"]),
    ("IS_PRIMITIVE(c.age)", ["1", "2", "3"]),
    ("STARTSWITH(c.name, 'A')", ["1"]),
    ("STARTSWITH(c.name, 'a')", []),
    ("STARTSWITH(c.name, 'a', true)", ["1"]),
    ("ENDSWITH(c.name, 'l')", ["3"]),
    ("CONTAINS(c.name, 'o')", ["2", "3"]),
    ("CONTAINS(c.name, 'B', true)", ["2"]),
    ("STRINGEQUALS(c.name, 'BOB', true)", ["2"]),
    ("STRINGEQUALS(c.name, 'BOB')", []),
    ("ARRAY_CONTAINS(c.tags, 'b')", ["1", "2"]),
    ("c.name LIKE 'A%'", ["1"]),
    ("c.name LIKE '_ob'", ["2"]),
    ("c.name LIKE '%r%'", ["3"]),
    ("c.name NOT LIKE '%o%'", ["1"]),
    ("c.age IN (25, 35)", ["2", "3"]),
    ("c.age BETWEEN 26 AND 35", ["1", "3"]),
    ("c.address.city = 'Oslo'", ["1"]),
    ('c["address"]["city"] = \'Bergen\'', ["2"]),
    ("c.name > 'B'", ["2", "3"]),
    ("c.active = true", ["1"]),
    ("c.active != true", ["2"]),
    # Comparing with an undefined property, or across types, is undefined: no match.
    ("c.missing = 1", []),
    ("c.age > 'a'", []),
    ("(c.age + 5) * 2 > 65", ["1", "3"]),
    ("c.age % 10 = 5", ["2", "3"]),
    ("c.age > 26 AND (c.active = true OR NOT IS_DEFINED(c.active))", ["1", "3"]),
]


@pytest.mark.parametrize(("where", "ids"), FILTERS, ids=[f for f, _ in FILTERS])
def test_filters(people, where, ids):
    assert values(people, f"SELECT VALUE c.id FROM c WHERE {where} ORDER BY c.id") == ids


# (projection, its values for the three people in id order)
PROJECTIONS = [
    ("LOWER(c.name)", ["alice", "bob", "carol"]),
    ("UPPER(c.name)", ["ALICE", "BOB", "CAROL"]),
    ("CONCAT(c.name, '-', ToString(c.age))", ["Alice-30", "bob-25", "Carol-35"]),
    ("SUBSTRING(c.name, 1, 2)", ["li", "ob", "ar"]),
    ("LEFT(c.name, 2)", ["Al", "bo", "Ca"]),
    ("RIGHT(c.name, 2)", ["ce", "ob", "ol"]),
    ("LENGTH(c.name)", [5, 3, 5]),
    ("INDEX_OF(c.name, 'o')", [-1, 1, 3]),
    ("REPLACE(c.name, 'o', '0')", ["Alice", "b0b", "Car0l"]),
    ("REVERSE(c.name)", ["ecilA", "bob", "loraC"]),
    ("TRIM(CONCAT('  ', c.name, '  '))", ["Alice", "bob", "Carol"]),
    ("LTRIM(CONCAT('  ', c.name))", ["Alice", "bob", "Carol"]),
    ("RTRIM(CONCAT(c.name, '  '))", ["Alice", "bob", "Carol"]),
    ("ARRAY_LENGTH(c.tags)", [2, 1, 0]),
    ("ARRAY_CONCAT(c.tags, ['z'])", [["a", "b", "z"], ["b", "z"], ["z"]]),
    ("ARRAY_SLICE(c.tags, 1)", [["b"], [], []]),
    ("ABS(c.score)", [2.5, 3.7, 10]),
    ("FLOOR(c.score)", [-3, 3, 10]),
    ("CEILING(c.score)", [-2, 4, 10]),
    ("ROUND(c.score)", [-3, 4, 10]),
    ("TRUNC(c.score)", [-2, 3, 10]),
    ("c.age > 28 ? 'older' : 'younger'", ["older", "younger", "older"]),
    # `??` takes the right side only when the left is undefined; null is defined.
    ("c.nick ?? 'none'", ["none", None, "none"]),
    ('{"n": c.name, "count": ARRAY_LENGTH(c.tags)}', [{"n": "Alice", "count": 2}, {"n": "bob", "count": 1}, {"n": "Carol", "count": 0}]),
    ("[c.id, c.age]", [["1", 30], ["2", 25], ["3", 35]]),
    ("(c.age + 5) * 2", [70, 60, 80]),
]


@pytest.mark.parametrize(("projection", "expected"), PROJECTIONS, ids=[p for p, _ in PROJECTIONS])
def test_projections(people, projection, expected):
    assert values(people, f"SELECT VALUE {projection} FROM c ORDER BY c.id") == expected


def test_query_clauses(people):
    # An undefined value is left out of a VALUE projection.
    assert values(people, "SELECT VALUE c.address.city FROM c ORDER BY c.id") == ["Oslo", "Bergen"]
    assert values(people, "SELECT TOP 2 VALUE c.id FROM c ORDER BY c.age") == ["2", "1"]
    assert values(people, "SELECT VALUE c.id FROM c ORDER BY c.id OFFSET 1 LIMIT 1") == ["2"]
    assert values(people, "SELECT VALUE c.id FROM c ORDER BY c.name DESC") == ["2", "3", "1"]
    assert sorted(values(people, "SELECT DISTINCT VALUE t FROM c JOIN t IN c.tags")) == ["a", "b"]
    assert values(people, "SELECT VALUE SUM(c.age) FROM c") == [90]
    assert values(people, "SELECT VALUE AVG(c.age) FROM c") == [30]
    assert values(people, "SELECT VALUE MIN(c.age) FROM c") == [25]
    assert values(people, "SELECT VALUE MAX(c.name) FROM c") == ["bob"]
    assert values(people, "SELECT VALUE COUNT(1) FROM c WHERE c.age > @min", parameters=[{"name": "@min", "value": 26}]) == [2]
    rows = values(people, "SELECT c.id, c.address.city AS city FROM c ORDER BY c.id")
    assert rows == [{"id": "1", "city": "Oslo"}, {"id": "2", "city": "Bergen"}, {"id": "3"}]
    star = values(people, "SELECT * FROM c WHERE c.id = '3'")
    assert {k: v for k, v in star[0].items() if not k.startswith("_")} == PEOPLE[2]
    nested = values(people, "SELECT VALUE c.id FROM c WHERE EXISTS(SELECT VALUE t FROM t IN c.tags WHERE t = 'a')")
    assert nested == ["1"]
    assert values(people, "SELECT VALUE ARRAY(SELECT VALUE UPPER(t) FROM t IN c.tags) FROM c ORDER BY c.id") == [["A", "B"], ["B"], []]


def test_cross_partition_queries(client):
    container = client.create_database_if_not_exists("querydb").create_container_if_not_exists("spread", partition_key=PartitionKey(path="/pk"))
    for i in range(10):
        container.upsert_item({"id": str(i), "pk": f"p{i % 3}", "n": i})
    query = lambda q: list(container.query_items(q, enable_cross_partition_query=True))
    assert query("SELECT VALUE c.n FROM c ORDER BY c.n DESC")[:3] == [9, 8, 7]
    assert query("SELECT VALUE COUNT(1) FROM c") == [10]
    assert query("SELECT VALUE SUM(c.n) FROM c WHERE c.pk = 'p0'") == [0 + 3 + 6 + 9]
    grouped = query("SELECT c.pk, COUNT(1) AS n FROM c GROUP BY c.pk")
    assert sorted((g["pk"], g["n"]) for g in grouped) == [("p0", 4), ("p1", 3), ("p2", 3)]
    assert sorted(query("SELECT DISTINCT VALUE c.pk FROM c")) == ["p0", "p1", "p2"]
    paged = container.query_items("SELECT VALUE c.n FROM c ORDER BY c.n", enable_cross_partition_query=True, max_item_count=3)
    assert [len(list(page)) for page in paged.by_page()] == [3, 3, 3, 1]


def test_databases_and_containers(cosmos, group, account, client):  # noqa: F811
    client.create_database("first")
    second = client.create_database("second", offer_throughput=400)
    assert sorted(d["id"] for d in client.list_databases()) == ["first", "second"]
    assert [d["id"] for d in client.query_databases("SELECT * FROM d WHERE d.id = 'second'")] == ["second"]
    with pytest.raises(exceptions.CosmosResourceExistsError):
        client.create_database("first")

    assert second.get_throughput().offer_throughput == 400
    second.replace_throughput(800)
    assert second.get_throughput().offer_throughput == 800

    container = second.create_container("items", partition_key=PartitionKey(path="/pk"), offer_throughput=500)
    second.create_container("other", partition_key=PartitionKey(path="/pk"))
    assert sorted(c["id"] for c in second.list_containers()) == ["items", "other"]
    assert container.get_throughput().offer_throughput == 500
    replaced = second.replace_container(container, partition_key=PartitionKey(path="/pk"), default_ttl=3600)
    assert replaced.read()["defaultTtl"] == 3600
    assert container.read()["partitionKey"]["paths"] == ["/pk"]

    # The same databases and containers on Resource Manager, and their removal.
    assert sorted(d.name for d in cosmos.sql_resources.list_sql_databases(group, account.name)) == ["first", "second"]
    second.delete_container("other")
    assert [c.name for c in cosmos.sql_resources.list_sql_containers(group, account.name, "second")] == ["items"]
    client.delete_database("first")
    assert [d.name for d in cosmos.sql_resources.list_sql_databases(group, account.name)] == ["second"]
    with pytest.raises(exceptions.CosmosResourceNotFoundError):
        client.get_database_client("first").read()


def test_patch_operations(client):
    container = client.create_database_if_not_exists("patchdb").create_container_if_not_exists("docs", partition_key=PartitionKey(path="/pk"))
    container.create_item({"id": "d", "pk": "p", "count": 1, "tags": ["a"], "old": "x", "nested": {"v": 1}})
    patched = container.patch_item(
        "d",
        partition_key="p",
        patch_operations=[
            {"op": "set", "path": "/status", "value": "new"},
            {"op": "replace", "path": "/nested/v", "value": 2},
            {"op": "remove", "path": "/old"},
            {"op": "add", "path": "/tags/0", "value": "first"},
            {"op": "add", "path": "/tags/-", "value": "last"},
            {"op": "incr", "path": "/count", "value": 5},
        ],
    )
    assert (patched["status"], patched["nested"], "old" in patched, patched["tags"], patched["count"]) == ("new", {"v": 2}, False, ["first", "a", "last"], 6)

    # A patch with a filter applies only while the filter holds.
    container.patch_item("d", partition_key="p", patch_operations=[{"op": "incr", "path": "/count", "value": 1}], filter_predicate="FROM c WHERE c.count = 6")
    with pytest.raises(exceptions.CosmosHttpResponseError) as refused:
        container.patch_item("d", partition_key="p", patch_operations=[{"op": "incr", "path": "/count", "value": 1}], filter_predicate="FROM c WHERE c.count = 6")
    assert refused.value.status_code == 412
    with pytest.raises(exceptions.CosmosHttpResponseError) as refused:
        container.patch_item("d", partition_key="p", patch_operations=[{"op": "replace", "path": "/absent", "value": 1}])
    assert refused.value.status_code == 400

    etag = container.read_item("d", partition_key="p")["_etag"]
    container.replace_item("d", {"id": "d", "pk": "p", "v": 1}, etag=etag, match_condition=MatchConditions.IfNotModified)
    with pytest.raises(exceptions.CosmosAccessConditionFailedError):
        container.replace_item("d", {"id": "d", "pk": "p", "v": 2}, etag=etag, match_condition=MatchConditions.IfNotModified)


def test_change_feed(client):
    container = client.create_database_if_not_exists("feeddb").create_container_if_not_exists("events", partition_key=PartitionKey(path="/pk"))
    for i in range(3):
        container.create_item({"id": str(i), "pk": "p", "v": i})
    container.upsert_item({"id": "0", "pk": "p", "v": 100})
    feed = list(container.query_items_change_feed(start_time="Beginning", partition_key="p"))
    # Each item appears once, at its latest version, in the order of last write.
    assert [(item["id"], item["v"]) for item in feed] == [("1", 1), ("2", 2), ("0", 100)]


def test_time_to_live(client):
    container = client.create_database_if_not_exists("ttldb").create_container_if_not_exists("short", partition_key=PartitionKey(path="/pk"), default_ttl=1)
    container.create_item({"id": "gone", "pk": "p"})
    container.create_item({"id": "kept", "pk": "p", "ttl": -1})
    time.sleep(2.2)
    assert sorted(i["id"] for i in container.query_items("SELECT * FROM c", partition_key="p")) == ["kept"]
    with pytest.raises(exceptions.CosmosResourceNotFoundError):
        container.read_item("gone", partition_key="p")


def test_order_by_across_types(client):
    """Mixed types sort as null, then booleans, then numbers, then strings."""
    container = client.create_database_if_not_exists("querydb").create_container_if_not_exists("mixed", partition_key=PartitionKey(path="/pk"))
    for i, v in enumerate(["b", 2, None, True, "a", False, 1]):
        container.upsert_item({"id": str(i), "pk": "p", "v": v})
    ordered = values(container, "SELECT VALUE c.v FROM c ORDER BY c.v")
    assert ordered == [None, False, True, 1, 2, "a", "b"]


def test_iterating_an_array_in_from(people):
    assert sorted(values(people, "SELECT VALUE t FROM t IN c.tags")) == ["a", "b", "b"]
    # The alias names each element, which can be filtered and projected.
    assert sorted(values(people, "SELECT VALUE UPPER(t) FROM t IN c.tags WHERE t != 'a'")) == ["B", "B"]
