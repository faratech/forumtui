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
		'/^thread-([0-9]{1,10})$/' => ['thread', 'thread_id'],
		'/^forum-([0-9]{1,10})$/' => ['thread', 'node_id'],
		// Conversations take attachments under a content type this client
		// does not upload to, so the context column is deliberately null:
		// an attachment key offered for a conversation draft is refused.
		'/^conversation-reply-([0-9]{1,10})$/' => ['conversation', null],
	];

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

		foreach (self::KINDS AS $pattern => [$family, $contextKey])
		{
			if (preg_match($pattern, $key, $m))
			{
				return [
					'family' => $family,
					'context_key' => $contextKey,
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

	public function actionGet()
	{
		$visitor = \XF::visitor();

		/** @var DraftFinder $finder */
		$finder = $this->finder(DraftFinder::class);
		$drafts = $finder->where('user_id', $visitor->user_id)
			->order('last_update', 'DESC')
			->limit(200)
			->fetch();

		$out = [];
		foreach ($drafts AS $draft)
		{
			$family = $this->scopeFamilyForKey($draft->draft_key);
			// Unknown kinds (a `report-` draft, or one from an add-on that
			// arrives later) are dropped rather than guessed at.
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
				'has_attachments' => !empty($extraData['attachment_hash']),
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

			/** @var DraftPlugin $draftPlugin */
			$draftPlugin = $this->plugin(DraftPlugin::class);
			// Without this the upload ages out from under the draft.
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
