<?php

namespace WindowsForum\TuiLink\Api\Controller;

use XF\ControllerPlugin\DraftPlugin;
use XF\Entity\Draft;
use XF\Finder\DraftFinder;
use XF\Repository\DraftRepository;

/**
 * Composer drafts for the wftui terminal client (wftui #716).
 *
 *   GET    /api/wf-tui-drafts  -> {drafts: [{key, message, title, last_update, has_attachments}]}
 *   POST   /api/wf-tui-drafts  {key, message, title?, attachment_key?}
 *   DELETE /api/wf-tui-drafts  {key}
 *
 * Stock XF has no draft endpoint at all — `XF\Entity\Draft` is written only by
 * `XF\ControllerPlugin\DraftPlugin` from the Pub controllers, i.e. the web
 * editor's autosave. Without this relay the TUI's drafts and the website's are
 * two stores that never meet.
 *
 * This class deliberately does NOT override `allowUnauthenticatedRequest`.
 * Its sibling `LinkController` does (it is the pre-login bootstrap), and these
 * endpoints read and write one user's unsent writing — the default, which is
 * "authenticated", is the only correct answer, and getting it by *not* writing
 * code is safer than getting it from an allowlist someone must maintain.
 *
 * No new OAuth scope: a scope minted today is absent from every 90-day token
 * already issued, so every live session would 403 until its owner
 * re-authorized (the wftui #695 lesson). Each draft kind instead asserts the
 * scope its own content type already implies — a token that may post to a
 * thread may certainly draft to it, so this grants nothing new.
 */
class DraftController extends \XF\Api\Controller\AbstractController
{
	/**
	 * The draft keys XF itself uses, from the entity relation conditions:
	 * `Thread::DraftReplies` (`thread-$thread_id`), `Forum::DraftThreads`
	 * (`forum-$node_id`) and `ConversationMaster::DraftReplies`
	 * (`conversation-reply-$conversation_id`).
	 *
	 * `report-` is a real fourth kind (`ReportController`) and is deliberately
	 * absent: the client has no reporting UI, and a moderator's report draft is
	 * not something to expose over an API nobody asked for it on.
	 */
	protected const KINDS = [
		'/^thread-([0-9]{1,10})$/' => ['thread', 'thread_id', 'thread-'],
		'/^forum-([0-9]{1,10})$/' => ['thread', 'node_id', 'forum-'],
		// Conversations take attachments under a content type this client
		// does not upload to, so the context column is deliberately null:
		// an attachment key offered for a conversation draft is refused.
		'/^conversation-reply-([0-9]{1,10})$/' => ['conversation', null, 'conversation-reply-'],
	];

	/** The most drafts one listing returns. */
	protected const LIST_LIMIT = 200;

	/**
	 * Every action here is one member's own writing. An API key acting as
	 * no user would read and write the guest's (user 0) draft rows.
	 */
	protected function preDispatchController($action, \XF\Mvc\ParameterBag $params)
	{
		$this->assertRegisteredUser();
	}

	/**
	 * Parse a draft key into its scope family, its attachment context column
	 * and its id — or null if it is not a key this relay handles.
	 *
	 * Validation and authorization are one decision, so they are one
	 * function: a key that does not parse can never reach a scope check that
	 * might pass.
	 */
	protected function parseKey($key)
	{
		if (!is_string($key) || strlen($key) > 75)
		{
			return null;
		}

		foreach (self::KINDS AS $pattern => [$family, $contextKey, $prefix])
		{
			if (preg_match($pattern, $key, $m))
			{
				return [
					'family' => $family,
					'context_key' => $contextKey,
					'prefix' => $prefix,
					'id' => (int) $m[1],
				];
			}
		}

		return null;
	}

	protected function scopeFamilyForKey($key)
	{
		$parsed = $this->parseKey($key);
		return $parsed ? $parsed['family'] : null;
	}

	/**
	 * Non-throwing `assertApiScope`, for the bulk read: an unscoped kind is
	 * omitted from the listing rather than failing the whole request, so a
	 * client without `conversation:read` still gets its thread drafts.
	 */
	protected function hasApiScope($scope)
	{
		$accessToken = \XF::accessToken();
		if ($accessToken && $accessToken->hasScope($scope))
		{
			return true;
		}

		$key = \XF::apiKey();
		if ($key && $key->hasScope($scope))
		{
			return true;
		}

		return false;
	}

	protected function assertValidKey($key)
	{
		$parsed = $this->parseKey($key);
		if ($parsed === null)
		{
			throw $this->exception($this->apiError(
				\XF::phrase('wf_tuilink.invalid_draft_key'),
				'wf_tuilink_invalid_draft_key',
				null,
				400
			));
		}

		return $parsed;
	}

	/**
	 * Permission to keep a draft for this target: the same check XF's own
	 * draft actions make before `DraftPlugin` writes (`assertViewableThread`,
	 * `assertViewableForum`, `assertViewableUserConversation`) — being able
	 * to see it. The token scope alone let a member park rows for threads,
	 * forums and conversations they cannot see (forumtui#126).
	 *
	 * A missing target and an unviewable one get the same 403: the relay
	 * does not confirm what exists, and a 404 here would read to the client
	 * as "no relay installed" and switch draft sync off for the session.
	 */
	protected function assertDraftTargetViewable(array $parsed)
	{
		$viewable = false;
		try
		{
			switch ($parsed['prefix'])
			{
				case 'thread-':
					$this->assertViewableApiRecord(\XF\Entity\Thread::class, $parsed['id']);
					$viewable = true;
					break;

				case 'forum-':
					$this->assertViewableApiRecord(\XF\Entity\Forum::class, $parsed['id']);
					$viewable = true;
					break;

				case 'conversation-reply-':
					$this->plugin(\XF\Api\ControllerPlugin\ConversationPlugin::class)
						->assertViewableUserConversation($parsed['id'], []);
					$viewable = true;
					break;
			}
		}
		catch (\XF\Mvc\Reply\Exception $e)
		{
			$viewable = false;
		}

		if (!$viewable)
		{
			throw $this->exception($this->apiError(
				\XF::phrase('do_not_have_permission'),
				'wf_tuilink_draft_target_unavailable',
				null,
				403
			));
		}
	}

	public function actionGet()
	{
		$visitor = \XF::visitor();

		/** @var DraftFinder $finder */
		$finder = $this->finder(DraftFinder::class);
		$finder->where('user_id', $visitor->user_id);

		// Only the kinds this token may read count against the limit
		// (forumtui#135): the limit used to be applied first and the
		// filtering after, so 200 newer `report-` (or unscoped) drafts hid
		// every older one this client could have adopted.
		$kinds = [];
		foreach (self::KINDS AS [$family, , $prefix])
		{
			if ($this->hasApiScope($family . ':read'))
			{
				$kinds[] = ['draft_key', 'LIKE', $finder->escapeLike($prefix, '?%')];
			}
		}
		if (!$kinds)
		{
			return $this->apiResult(['success' => true, 'drafts' => []]);
		}
		$drafts = $finder->whereOr($kinds)
			->order('last_update', 'DESC')
			->limit(self::LIST_LIMIT)
			->fetch();

		// A web draft's attachments are temp uploads that age out; say a
		// draft has attachments only while they still exist (forumtui#133).
		$hashes = [];
		foreach ($drafts AS $draft)
		{
			$hash = ($draft->extra_data ?: [])['attachment_hash'] ?? '';
			if (is_string($hash) && $hash !== '')
			{
				$hashes[$hash] = true;
			}
		}
		$liveHashes = [];
		if ($hashes)
		{
			$db = $this->app()->db();
			$liveHashes = array_fill_keys($db->fetchAllColumn(
				'SELECT DISTINCT temp_hash FROM xf_attachment WHERE temp_hash IN (' . $db->quote(array_keys($hashes)) . ')'
			), true);
		}

		$out = [];
		foreach ($drafts AS $draft)
		{
			$family = $this->scopeFamilyForKey($draft->draft_key);
			// A key that matched a prefix but not its exact shape is still
			// dropped rather than guessed at.
			if ($family === null || !$this->hasApiScope($family . ':read'))
			{
				continue;
			}

			$extraData = $draft->extra_data ?: [];

			$out[] = [
				'key' => $draft->draft_key,
				'message' => $draft->message,
				'title' => isset($extraData['title']) ? (string) $extraData['title'] : '',
				'last_update' => $draft->last_update,
				// The attachment hash itself is NOT returned. It is an
				// `xf_attachment.temp_hash`, and the API's own attachment keys
				// are `ApiAttachmentKey` rows that WRAP a hash —
				// `ApiAttachmentKey::_preSave` generates both and refuses to
				// let either be set, so a hash cannot be turned back into a
				// key a client could use. Saying whether files exist is
				// honest; handing over a hash the client cannot spend is not.
				'has_attachments' => !empty($extraData['attachment_hash'])
					&& isset($liveHashes[$extraData['attachment_hash']]),
			];
		}

		return $this->apiResult(['success' => true, 'drafts' => $out]);
	}

	public function actionPost()
	{
		$key = $this->filter('key', 'str');
		$parsed = $this->assertValidKey($key);
		$this->assertApiScope($parsed['family'] . ':write');

		$visitor = \XF::visitor();
		$message = $this->filter('message', 'str');
		if (strlen(trim($message)))
		{
			$this->assertDraftTargetViewable($parsed);
		}

		/** @var DraftRepository $draftRepo */
		$draftRepo = $this->repository(DraftRepository::class);

		// An empty draft deletes, exactly as `DraftPlugin::updateMessageDraft`
		// does — otherwise clearing a composer would leave a blank draft for
		// the web editor to restore over the user's next attempt.
		if (!strlen(trim($message)))
		{
			$draftRepo->deleteDraft($key, $visitor);
			return $this->apiResult(['success' => true, 'action' => 'delete']);
		}

		$draft = $draftRepo->getDraftByKeyAndUser($key, $visitor);
		if (!$draft)
		{
			$draft = $this->em()->create(Draft::class);
			$draft->draft_key = $key;
			$draft->user_id = $visitor->user_id;
		}

		// Merge rather than replace: the web editor writes prefix_id,
		// discussion_type, tags and custom_fields into the same blob, and a
		// draft round-tripping through the TUI must not silently drop the
		// prefix somebody chose in the browser.
		$extraData = $draft->extra_data ?: [];
		$extraData['title'] = $this->filter('title', 'str');

		$attachmentKey = $this->filter('attachment_key', 'str');
		if (strlen($attachmentKey))
		{
			if ($parsed['context_key'] === null)
			{
				throw $this->exception($this->apiError(
					\XF::phrase('wf_tuilink.draft_takes_no_attachments'),
					'wf_tuilink_draft_takes_no_attachments',
					null,
					400
				));
			}

			// Resolves the opaque API key to its temp hash, checking the
			// content type, the owning user and the context on the way
			// through — so a key minted for one thread cannot be parked on
			// another thread's draft. That hash is what the web editor reads,
			// so files attached in the TUI show up in the browser's draft.
			$extraData['attachment_hash'] = $this->getAttachmentTempHashFromKey(
				$attachmentKey,
				'post',
				[$parsed['context_key'] => $parsed['id']]
			);

		}

		// Without this the upload ages out from under the draft — whether
		// the hash came with this save or was attached in the browser and
		// merely kept (forumtui#133). A draft resumed from the website has
		// no API key to send, so every terminal save arrives key-less, and
		// refreshing only on the key path let its files expire while the
		// draft was being edited here.
		if (!empty($extraData['attachment_hash']))
		{
			/** @var DraftPlugin $draftPlugin */
			$draftPlugin = $this->plugin(DraftPlugin::class);
			$draftPlugin->refreshTempAttachments($extraData['attachment_hash']);
		}

		$draft->message = $message;
		$draft->extra_data = $extraData;
		$draft->save();

		return $this->apiResult([
			'success' => true,
			'action' => 'save',
			'last_update' => $draft->last_update,
		]);
	}

	public function actionDelete()
	{
		$key = $this->filter('key', 'str');
		$parsed = $this->assertValidKey($key);
		$this->assertApiScope($parsed['family'] . ':write');

		// Scoped to the visitor by the repository, so one user's key can never
		// reach another's row.
		$deleted = $this->repository(DraftRepository::class)
			->deleteDraft($key, \XF::visitor());

		return $this->apiResult(['success' => true, 'deleted' => $deleted]);
	}
}
