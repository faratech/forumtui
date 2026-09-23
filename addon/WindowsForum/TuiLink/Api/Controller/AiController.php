<?php

namespace WindowsForum\TuiLink\Api\Controller;

/**
 * "Ask the AI" for the wftui terminal client.
 *
 *   GET  /api/wf-tui-ai  -> the member's AI usage today (chat.php `getUsage`)
 *   POST /api/wf-tui-ai  {message, conversation_id, reset?, history[]?}
 *                        -> text/event-stream, chat.php's own frames verbatim
 *
 * The site's assistant lives in /chat.php, which authenticates a browser: the
 * XF session cookie, a Turnstile approval cookie, and `_xfToken` on its
 * product actions. A terminal holds an OAuth bearer and none of those. This
 * route is the bridge: the API app has already turned the bearer into a
 * visitor, and the request is forwarded to chat.php over loopback carrying
 * that identity signed (see `sign()`), so the turn ledger, conversation
 * leases, blocklist, quotas and the model call all stay in the one place
 * that already owns them rather than being forked into this add-on.
 *
 * chat.php trusts the signature only from loopback, within 30 s, once per
 * nonce, and only for sending a message, `getUsage` and `getUserData` — no
 * saved-conversation or share mutation can be reached through here.
 *
 * Authenticated by default (no `allowUnauthenticatedRequest` override), and
 * no new OAuth scope (the wftui #695 lesson): asking the assistant asserts
 * `user:read`, which every terminal token already carries.
 */
class AiController extends \XF\Api\Controller\AbstractController
{
	/** chat.php, reached over loopback so the signed header is trusted. */
	protected const CHAT_URL = 'http://127.0.0.1/chat.php';

	/** chat.php's own caps (MAX_MESSAGE_LENGTH, CHATPAGE_HISTORY_SEED_LIMIT). */
	protected const MAX_MESSAGE_BYTES = 4096;
	protected const MAX_HISTORY_ITEMS = 20;
	protected const MAX_HISTORY_ITEM_BYTES = 4000;

	/** chat.php's provider call allows 180 s; leave room for its epilogue. */
	protected const STREAM_TIMEOUT = 200;

	protected function preDispatchController($action, \XF\Mvc\ParameterBag $params)
	{
		$this->assertApiScope('user:read');
		$this->assertRegisteredUser();
	}

	public function actionGet()
	{
		$response = $this->forward(['action' => 'getUsage'], false);
		$status = $response->getStatusCode();
		$data = json_decode((string) $response->getBody(), true);
		if ($status !== 200 || !is_array($data))
		{
			return $this->relayError($status, is_array($data) ? $data : [], $response->getHeaderLine('Retry-After'));
		}

		return $this->apiResult(['usage' => $data]);
	}

	public function actionPost()
	{
		$message = trim($this->filter('message', 'str'));
		if ($message === '' || strlen($message) > self::MAX_MESSAGE_BYTES)
		{
			return $this->apiError('The question must be 1-4096 bytes.', 'wf_tui_ai_invalid_message', null, 400);
		}

		$conversationId = $this->filter('conversation_id', 'str');
		if (!preg_match('/^[A-Za-z0-9_-]{1,128}$/', $conversationId))
		{
			return $this->apiError('A conversation id is required.', 'wf_tui_ai_invalid_conversation', null, 400);
		}

		$body = [
			'message' => $message,
			'client_conversation_id' => $conversationId,
			'expected_identity_id' => $this->identityId(\XF::visitor()->user_id),
		];
		if ($this->filter('reset', 'bool'))
		{
			$body['reset_conversation'] = true;
		}

		// chat.php answers `history_required` (409) when it has lost a
		// conversation the client still remembers; the client resends once
		// with the transcript, exactly as the web client does.
		$history = [];
		foreach (array_slice($this->filter('history', 'array'), -self::MAX_HISTORY_ITEMS) AS $item)
		{
			if (!is_array($item) || !in_array($item['role'] ?? '', ['user', 'assistant'], true))
			{
				continue;
			}
			$content = substr(trim((string) ($item['content'] ?? '')), 0, self::MAX_HISTORY_ITEM_BYTES);
			if ($content !== '')
			{
				$history[] = ['role' => $item['role'], 'content' => $content];
			}
		}
		if ($history)
		{
			$body['history'] = $history;
		}
		else if ($this->filter('has_local_history', 'bool'))
		{
			$body['has_local_history'] = true;
		}

		$turnId = (string) $this->request->getServer('HTTP_X_WF_TURN_ID', '');
		$response = $this->forward(
			$body,
			true,
			preg_match('/^[A-Za-z0-9_-]{1,64}$/', $turnId) ? $turnId : null
		);

		$status = $response->getStatusCode();
		$contentType = strtolower($response->getHeaderLine('Content-Type'));
		if ($status !== 200 || strpos($contentType, 'text/event-stream') !== 0)
		{
			$data = json_decode((string) $response->getBody(), true);
			return $this->relayError($status, is_array($data) ? $data : [], $response->getHeaderLine('Retry-After'));
		}

		$this->pump($response->getBody());
		exit;
	}

	/**
	 * chat.php's identity marker (`wfChatIdentityId` in wf_chat_predicates.php).
	 * The bearer IS the identity here, so the relay supplies it rather than
	 * making the client fetch and echo it back.
	 */
	protected function identityId($userId)
	{
		return hash_hmac('sha256', 'wf-chat-identity|' . $userId, (string) \XF::config('secretKey'));
	}

	/**
	 * Derived rather than stored: config.php is immutable on this server, and
	 * chat.php derives the same key from the same secretKey. The label keeps
	 * it distinct from every other use of that key.
	 */
	protected function relayKey()
	{
		return hash_hmac('sha256', 'wf-tui-relay|v1', (string) \XF::config('secretKey'));
	}

	protected function sign($userId, $rawBody)
	{
		$ts = \XF::$time;
		$nonce = bin2hex(random_bytes(16));
		$sig = hash_hmac('sha256', "v1|{$userId}|{$ts}|{$nonce}|" . hash('sha256', $rawBody), $this->relayKey());
		return "v1.{$userId}.{$ts}.{$nonce}.{$sig}";
	}

	/**
	 * @return \Psr\Http\Message\ResponseInterface
	 */
	protected function forward(array $body, $stream, $turnId = null)
	{
		$raw = json_encode($body, JSON_UNESCAPED_SLASHES | JSON_UNESCAPED_UNICODE);
		$host = parse_url((string) \XF::options()->boardUrl, PHP_URL_HOST) ?: 'windowsforum.com';
		$headers = [
			'Host' => $host,
			'Content-Type' => 'application/json',
			'Accept' => $stream ? 'text/event-stream' : 'application/json',
			'X-WF-Relay' => $this->sign(\XF::visitor()->user_id, $raw),
		];
		if ($turnId !== null)
		{
			$headers['X-WF-Turn-Id'] = $turnId;
		}

		$client = new \GuzzleHttp\Client([
			'http_errors' => false,
			'proxy' => '',
			'connect_timeout' => 5,
			'timeout' => $stream ? self::STREAM_TIMEOUT : 10,
			'read_timeout' => $stream ? 60 : 10,
		]);

		try
		{
			return $client->post(self::CHAT_URL, [
				'headers' => $headers,
				'body' => $raw,
				'stream' => (bool) $stream,
			]);
		}
		catch (\Throwable $e)
		{
			\XF::logException($e, false, 'wftui AI relay: ');
			throw $this->exception($this->apiError(
				'The assistant is unavailable right now.',
				'wf_tui_ai_unavailable',
				null,
				502
			));
		}
	}

	/**
	 * A refusal before the stream started, in XF's own error envelope so the
	 * client's `error_from_response` reads it like any other. chat.php's
	 * `code` is kept (rate_limited, history_required, conversation_busy, ...)
	 * because the client acts on it; Retry-After is carried through.
	 */
	protected function relayError($status, array $data, $retryAfter)
	{
		$status = ($status >= 400 && $status < 600) ? $status : 502;
		$code = is_string($data['code'] ?? null) && $data['code'] !== ''
			? $data['code']
			: 'wf_tui_ai_http_' . $status;
		$message = (string) ($data['message'] ?? $data['error'] ?? 'The assistant refused the request.');

		$reply = $this->apiError($message, $code, null, $status);
		if (ctype_digit((string) $retryAfter))
		{
			$reply->setResponseHeaders(['Retry-After' => (string) $retryAfter]);
		}
		return $reply;
	}

	/**
	 * Pass chat.php's SSE through unchanged. XF's reply pipeline buffers a
	 * whole body, so this writes directly and the action exits afterwards.
	 */
	protected function pump(\Psr\Http\Message\StreamInterface $body)
	{
		while (ob_get_level() > 0)
		{
			ob_end_clean();
		}
		ignore_user_abort(false);

		http_response_code(200);
		header('Content-Type: text/event-stream; charset=utf-8');
		header('Cache-Control: no-cache, no-store, no-transform');
		header('X-Accel-Buffering: no');
		header('X-Content-Type-Options: nosniff');

		while (!$body->eof())
		{
			$chunk = $body->read(8192);
			if ($chunk === '')
			{
				continue;
			}
			echo $chunk;
			flush();
			if (connection_aborted())
			{
				break;
			}
		}
		$body->close();
	}
}
