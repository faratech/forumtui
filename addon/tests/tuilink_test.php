<?php
/**
 * TuiLink add-on unit tests — no XenForo, no database server, no Redis:
 *
 *     php -n addon/tests/tuilink_test.php            # the add-on source here
 *     php -n addon/tests/tuilink_test.php <addon dir> # any copy, e.g. the deployed one
 *
 * The XenForo surface the add-on touches is stubbed below, one small fake per
 * collaborator; LinkStore runs its own SQL against an in-memory SQLite table
 * shaped like `xf_wf_tuilink_link`. Exits non-zero when any test fails.
 */

namespace XF\Db {
	class DuplicateKeyException extends \Exception {}
	abstract class AbstractAdapter {}
}

namespace {
	$GLOBALS['addonDir'] = rtrim($argv[1] ?? (__DIR__ . '/../WindowsForum/TuiLink'), '/');

	final class XF
	{
		public static $time = 1_800_000_000;
		public static $db;
		public static $visitor;
		public static $scopes = [];

		public static function db() { return self::$db; }
		public static function visitor() { return self::$visitor; }
		public static function config($key) { return $key === 'globalSalt' ? 'test-salt' : null; }
		public static function phrase($key) { return $key; }
		public static function generateRandomString($length) { return substr(bin2hex(random_bytes($length)), 0, $length); }
		public static function isApiCheckingPermissions() { return true; }
		public static function apiKey() { return null; }
		public static function accessToken()
		{
			return new class {
				public function hasScope($scope) { return in_array($scope, \XF::$scopes, true); }
			};
		}
	}

	/** `XF\Db\AbstractAdapter`'s calls that LinkStore makes, over PDO SQLite. */
	final class FakeDb extends \XF\Db\AbstractAdapter
	{
		public $pdo;
		public $liveTempHashes = [];

		public function __construct()
		{
			$this->pdo = new PDO('sqlite::memory:');
			$this->pdo->setAttribute(PDO::ATTR_ERRMODE, PDO::ERRMODE_EXCEPTION);
			$this->pdo->exec('CREATE TABLE xf_wf_tuilink_link (link_id BLOB PRIMARY KEY, state BLOB NOT NULL DEFAULT \'\',
				state_hash BLOB NOT NULL UNIQUE, challenge BLOB NOT NULL, code BLOB DEFAULT NULL,
				denied INTEGER NOT NULL DEFAULT 0, expiry_date INTEGER NOT NULL)');
		}

		public function update($table, array $cols, $where, $params = [])
		{
			$set = implode(', ', array_map(fn($c) => "$c = ?", array_keys($cols)));
			$stmt = $this->pdo->prepare("UPDATE $table SET $set WHERE $where");
			$stmt->execute(array_merge(array_values($cols), $params));
			return $stmt->rowCount();
		}

		public function insert($table, array $row)
		{
			$names = implode(', ', array_keys($row));
			$marks = implode(', ', array_fill(0, count($row), '?'));
			$this->pdo->prepare("INSERT INTO $table ($names) VALUES ($marks)")->execute(array_values($row));
		}

		public function delete($table, $where, $params = [])
		{
			$stmt = $this->pdo->prepare("DELETE FROM $table WHERE $where");
			$stmt->execute((array) $params);
			return $stmt->rowCount();
		}

		public function fetchRow($sql, $params = [])
		{
			$stmt = $this->pdo->prepare($sql);
			$stmt->execute($params);
			return $stmt->fetch(PDO::FETCH_ASSOC) ?: false;
		}

		public function fetchOne($sql, $params = [])
		{
			$stmt = $this->pdo->prepare($sql);
			$stmt->execute($params);
			return $stmt->fetchColumn();
		}

		public function quote($value)
		{
			return implode(',', array_map(fn($v) => "'" . str_replace("'", "''", $v) . "'", (array) $value));
		}

		/** Only the xf_attachment temp-hash lookup DraftController makes. */
		public function fetchAllColumn($sql)
		{
			preg_match_all("/'([^']*)'/", $sql, $m);
			return array_values(array_intersect($m[1], $this->liveTempHashes));
		}
	}

	/** A Redis that can be told an EXPIRE never happened. */
	final class FakeRedis
	{
		public $values = [];
		public $ttls = [];

		public function incr($key) { return $this->values[$key] = ($this->values[$key] ?? 0) + 1; }
		public function expire($key, $seconds) { if (isset($this->values[$key])) { $this->ttls[$key] = $seconds; } return true; }
		public function ttl($key) { return isset($this->values[$key]) ? ($this->ttls[$key] ?? -1) : -2; }
	}
}

namespace WindowsForum {
	final class SharedRedis
	{
		public static $redis;
		public static function raw() { return self::$redis; }
	}
}

namespace XF\Mvc {
	class ParameterBag {}
}

namespace XF\Mvc\Reply {
	class Exception extends \Exception
	{
		public $reply;
		public function __construct($reply) { parent::__construct(is_array($reply) ? ($reply['error'] ?? 'reply') : 'reply'); $this->reply = $reply; }
	}
}

namespace XF\Api\Controller {
	/** The slice of XenForo's API controller that DraftController calls. */
	abstract class AbstractController
	{
		public $input = [];
		public $world;

		public function __construct(array $input, $world) { $this->input = $input; $this->world = $world; }
		protected function preDispatchController($action, \XF\Mvc\ParameterBag $params) {}
		public function preDispatch($action) { $this->preDispatchController($action, new \XF\Mvc\ParameterBag()); }

		public function filter($key, $type) { return (string) ($this->input[$key] ?? ''); }
		public function app() { return $this->world; }
		public function repository($class) { return $this->world->drafts; }
		public function em() { return $this->world; }
		public function finder($class) { return new \FakeDraftFinder($this->world->drafts->rows); }
		public function plugin($class)
		{
			return str_ends_with($class, 'ConversationPlugin') ? $this->world->conversations : $this->world->draftPlugin;
		}
		public function apiResult(array $data) { return $data; }
		public function apiError($message, $code, $params = null, $status = 400) { return ['error' => $code, 'status' => $status]; }
		public function noPermission($error = null) { return ['error' => 'no_permission', 'status' => 403]; }
		public function notFound($message = null) { return ['error' => 'not_found', 'status' => 404]; }
		public function exception($reply) { return new \XF\Mvc\Reply\Exception($reply); }
		public function assertApiScope($scope)
		{
			if (!in_array($scope, \XF::$scopes, true)) { throw $this->exception($this->apiError('', 'missing_scope', null, 403)); }
		}
		public function assertRegisteredUser()
		{
			if (!\XF::visitor()->user_id) { throw $this->exception($this->noPermission()); }
		}
		public function assertViewableApiRecord($class, $id)
		{
			if (!in_array($id, $this->world->viewable[$class] ?? [], true)) { throw $this->exception($this->noPermission()); }
			return (object) ['id' => $id];
		}
		public function getAttachmentTempHashFromKey($key, $type, array $context) { return 'hash-of-' . $key; }
	}
}

namespace {
	final class FakeDraft
	{
		public $draft_key;
		public $user_id;
		public $message = '';
		public $extra_data = [];
		public $last_update = 0;
		public $repo;

		public function save()
		{
			$this->last_update = \XF::$time;
			if (!in_array($this, $this->repo->rows, true)) { $this->repo->rows[] = $this; }
		}
	}

	final class FakeDraftRepo
	{
		public $rows = [];

		public function getDraftByKeyAndUser($key, $user)
		{
			foreach ($this->rows AS $row) { if ($row->draft_key === $key && $row->user_id === $user->user_id) { return $row; } }
			return null;
		}

		public function deleteDraft($key, $user)
		{
			$before = count($this->rows);
			$this->rows = array_values(array_filter($this->rows, fn($r) => !($r->draft_key === $key && $r->user_id === $user->user_id)));
			return $before !== count($this->rows);
		}

		public function add($key, $userId, $lastUpdate, array $extra = [])
		{
			$d = new FakeDraft();
			[$d->draft_key, $d->user_id, $d->last_update, $d->extra_data, $d->message, $d->repo] = [$key, $userId, $lastUpdate, $extra, "draft $key", $this];
			$this->rows[] = $d;
			return $d;
		}
	}

	/** `XF\Mvc\Entity\Finder`, for where / whereOr(LIKE) / order / limit. */
	final class FakeDraftFinder
	{
		private $rows;
		private $filters = [];
		private $limit = null;

		public function __construct(array $rows) { $this->rows = $rows; }
		public function escapeLike($value, $format = null)
		{
			$escaped = addcslashes($value, '%_\\');
			return $format === null ? $escaped : str_replace('?', $escaped, $format);
		}
		public function where($col, $value) { $this->filters[] = fn($r) => $r->$col === $value; return $this; }
		public function whereOr(array $conditions)
		{
			$this->filters[] = function ($r) use ($conditions) {
				foreach ($conditions AS [$col, $op, $pattern])
				{
					$regex = '/^' . strtr(preg_quote(stripslashes($pattern), '/'), ['%' => '.*', '_' => '.']) . '$/s';
					if ($op === 'LIKE' && preg_match($regex, $r->$col)) { return true; }
				}
				return false;
			};
			return $this;
		}
		public function order($col, $dir) { usort($this->rows, fn($a, $b) => $b->$col <=> $a->$col); return $this; }
		public function limit($n) { $this->limit = $n; return $this; }
		public function fetch()
		{
			$out = array_values(array_filter($this->rows, fn($r) => array_reduce($this->filters, fn($ok, $f) => $ok && $f($r), true)));
			return $this->limit === null ? $out : array_slice($out, 0, $this->limit);
		}
	}

	final class World
	{
		public $drafts;
		public $draftPlugin;
		public $conversations;
		public $viewable = [];
		public $db;

		public function __construct()
		{
			$this->drafts = new FakeDraftRepo();
			$this->db = new FakeDb();
			$this->draftPlugin = new class {
				public $refreshed = [];
				public function refreshTempAttachments($hashes) { $this->refreshed[] = $hashes; }
			};
			$world = $this;
			$this->conversations = new class($world) {
				public $mine = [];
				private $world;
				public function __construct($world) { $this->world = $world; }
				public function assertViewableUserConversation($id, $with)
				{
					if (!in_array($id, $this->mine, true)) { throw new \XF\Mvc\Reply\Exception(['error' => 'not_found', 'status' => 404]); }
					return (object) ['conversation_id' => $id];
				}
			};
		}
		public function db() { return $this->db; }
		public function create($class)
		{
			$d = new FakeDraft();
			$d->repo = $this->drafts;
			return $d;
		}
	}

	require $GLOBALS['addonDir'] . '/Config.php';
	require $GLOBALS['addonDir'] . '/Service/LinkStoreException.php';
	require $GLOBALS['addonDir'] . '/Service/LinkStore.php';
	require $GLOBALS['addonDir'] . '/Api/Controller/DraftController.php';

	use WindowsForum\TuiLink\Service\LinkStore;
	use WindowsForum\TuiLink\Api\Controller\DraftController;

	$failures = 0;
	function check($ok, $what)
	{
		global $failures;
		if (!$ok) { $failures++; fwrite(STDERR, "  FAIL: $what\n"); }
	}
	function run($name, callable $test)
	{
		global $failures;
		$before = $failures;
		XF::$db = new FakeDb();
		XF::$visitor = (object) ['user_id' => 7];
		XF::$scopes = ['thread:read', 'thread:write', 'conversation:read', 'conversation:write'];
		WindowsForum\SharedRedis::$redis = new FakeRedis();
		try { $test(); }
		catch (\Throwable $e) { $failures++; fwrite(STDERR, '  FAIL: ' . get_class($e) . ': ' . $e->getMessage() . "\n"); }
		echo ($failures === $before ? 'ok   ' : 'FAIL ') . $name . "\n";
	}
	/** Run a controller action; a thrown reply comes back as the reply. */
	function act(World $world, array $input, $action)
	{
		$c = new DraftController($input, $world);
		try { $c->preDispatch($action); return $c->{'action' . $action}(); }
		catch (\XF\Mvc\Reply\Exception $e) { return $e->reply; }
	}

	// forumtui#134: a consumed code is never re-attached by a replayed /tui-done.
	run('a consumed code cannot be re-armed', function () {
		$id = LinkStore::create(str_repeat('s', 16), str_repeat('c', 43));
		check(LinkStore::attachCode($id, 'code-1'), 'first attach');
		check(LinkStore::get($id)['code'] === 'code-1', 'the code is waiting');
		check(LinkStore::consumeCode($id, 'code-1'), 'the poll consumes it');
		check(!LinkStore::attachCode($id, 'code-1'), 'a replay of /tui-done does not re-attach');
		check(empty(LinkStore::get($id)['code']), 'the next poll still finds no code');
		check(LinkStore::isConsumed($id), 'the record says it was consumed');
		check(!LinkStore::consumeCode($id, 'code-1'), 'and cannot be consumed twice');
		$fresh = LinkStore::create(str_repeat('t', 16), str_repeat('c', 43));
		check(!LinkStore::isConsumed($fresh), 'a fresh record is not consumed');
		check(LinkStore::attachCode($fresh, 'a') && LinkStore::attachCode($fresh, 'b'), 'before consumption a re-approval may replace the code');
	});

	// forumtui#125: a register budget with no TTL is repaired, not permanent.
	run('a register counter that lost its TTL gets one back', function () {
		$redis = WindowsForum\SharedRedis::$redis;
		check(LinkStore::reserveRegisterSlot('203.0.113.9'), 'the first registration is allowed');
		$key = array_key_first($redis->values);
		check($redis->ttl($key) === LinkStore::REGISTER_WINDOW, 'a new counter gets the window');
		// What a worker that died between INCR and EXPIRE leaves behind.
		$redis->values[$key] = 40;
		unset($redis->ttls[$key]);
		check(!LinkStore::reserveRegisterSlot('203.0.113.9'), 'over budget is refused');
		check($redis->ttl($key) === LinkStore::REGISTER_WINDOW, 'but the counter now expires (ttl ' . $redis->ttl($key) . ')');
	});

	// forumtui#126: a draft is kept only for a target the member can view.
	run('drafts are refused for targets the member cannot view', function () {
		$w = new World();
		$w->viewable = [\XF\Entity\Thread::class => [6], \XF\Entity\Forum::class => [2]];
		$w->conversations->mine = [3];
		foreach (['thread-5', 'forum-9', 'conversation-reply-4'] AS $key)
		{
			$reply = act($w, ['key' => $key, 'message' => 'secret plans'], 'Post');
			check(($reply['error'] ?? null) === 'wf_tuilink_draft_target_unavailable' && $reply['status'] === 403, "$key is refused with the relay's 403");
		}
		check($w->drafts->rows === [], 'nothing was stored for them');
		foreach (['thread-6', 'forum-2', 'conversation-reply-3'] AS $key)
		{
			$reply = act($w, ['key' => $key, 'message' => 'hello'], 'Post');
			check(($reply['action'] ?? null) === 'save', "$key is saved");
		}
		check(count($w->drafts->rows) === 3, 'three drafts stored');
		$w->drafts->add('thread-5', 7, 1);
		$reply = act($w, ['key' => 'thread-5', 'message' => ''], 'Post');
		check(($reply['action'] ?? null) === 'delete', 'an existing draft for an unviewable target can still be cleared');
		XF::$visitor = (object) ['user_id' => 0];
		$reply = act($w, [], 'Get');
		check(($reply['error'] ?? null) === 'no_permission', 'a request acting as nobody is refused');
	});

	// forumtui#133: a hash kept from the browser is refreshed on every save,
	// and has_attachments reports files that still exist.
	run('web-origin draft attachments are refreshed and reported truthfully', function () {
		$w = new World();
		$w->viewable = [\XF\Entity\Thread::class => [6]];
		$w->drafts->add('thread-6', 7, 10, ['attachment_hash' => 'webhash', 'prefix_id' => 4]);
		$reply = act($w, ['key' => 'thread-6', 'message' => 'edited in the terminal'], 'Post');
		check(($reply['action'] ?? null) === 'save', 'saved');
		check(in_array('webhash', $w->draftPlugin->refreshed, true), 'the kept hash was refreshed');
		$row = $w->drafts->getDraftByKeyAndUser('thread-6', XF::$visitor);
		check(($row->extra_data['attachment_hash'] ?? null) === 'webhash' && ($row->extra_data['prefix_id'] ?? null) === 4, 'extra data merged, not replaced');
		$w->db->liveTempHashes = ['webhash'];
		$list = act($w, [], 'Get');
		check(($list['drafts'][0]['has_attachments'] ?? null) === true, 'live uploads are reported');
		$w->db->liveTempHashes = [];
		$list = act($w, [], 'Get');
		check(($list['drafts'][0]['has_attachments'] ?? null) === false, 'pruned uploads are not');
		$w->drafts->rows = [];
		act($w, ['key' => 'thread-6', 'message' => 'x', 'attachment_key' => 'k1'], 'Post');
		check(in_array('hash-of-k1', $w->draftPlugin->refreshed, true), 'a key-carried hash is still refreshed');
	});

	// forumtui#135: the limit counts only drafts this token may read.
	run('the draft list limit applies after kind and scope filtering', function () {
		$w = new World();
		for ($i = 0; $i < 250; $i++) { $w->drafts->add("report-$i", 7, 1000 + $i); }
		for ($i = 0; $i < 50; $i++) { $w->drafts->add("conversation-reply-$i", 7, 900 + $i); }
		$w->drafts->add('thread-1', 7, 5);
		$w->drafts->add('thread-2', 8, 6);
		XF::$scopes = ['thread:read'];
		$list = act($w, [], 'Get');
		$keys = array_column($list['drafts'] ?? [], 'key');
		check($keys === ['thread-1'], 'the old thread draft is returned past 300 newer unreturnable ones: ' . json_encode($keys));
		XF::$scopes = ['thread:read', 'conversation:read'];
		$keys = array_column(act($w, [], 'Get')['drafts'] ?? [], 'key');
		check(count($keys) === 51 && in_array('thread-1', $keys, true), 'with conversation:read both kinds come back (' . count($keys) . ')');
		XF::$scopes = [];
		check((act($w, [], 'Get')['drafts'] ?? null) === [], 'no read scope, no drafts');
	});

	echo $failures ? "$failures failure(s)\n" : "all passed\n";
	exit($failures ? 1 : 0);
}
