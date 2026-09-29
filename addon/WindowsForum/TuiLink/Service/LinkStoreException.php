<?php

namespace WindowsForum\TuiLink\Service;

/**
 * A register refusal that is the caller's to see, not a server fault:
 * carries a short machine code the controller maps to an API error (#63).
 */
final class LinkStoreException extends \RuntimeException
{
	/** @var string */
	private $code_name;

	public function __construct(string $code_name)
	{
		parent::__construct("wf_tuilink_link_store: {$code_name}");
		$this->code_name = $code_name;
	}

	public function codeName(): string
	{
		return $this->code_name;
	}
}
