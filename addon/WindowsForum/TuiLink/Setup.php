<?php

namespace WindowsForum\TuiLink;

use XF\AddOn\AbstractSetup;
use XF\AddOn\StepRunnerInstallTrait;
use XF\AddOn\StepRunnerUninstallTrait;
use XF\AddOn\StepRunnerUpgradeTrait;
use XF\Db\Schema\Alter;
use XF\Db\Schema\Create;

class Setup extends AbstractSetup
{
	use StepRunnerInstallTrait;
	use StepRunnerUpgradeTrait;
	use StepRunnerUninstallTrait;

	public function installStep1(): void
	{
		$this->createLinkTable();
	}

	/** 1.0.x kept login records in the global cache; 1.1.0 keeps them here. */
	public function upgrade1010070Step1(): void
	{
		$this->createLinkTable();
	}

	/** 1.1.0 stored only the state's hash; the authorize URL needs it verbatim. */
	public function upgrade1010170Step1(): void
	{
		$this->schemaManager()->alterTable('xf_wf_tuilink_link', function (Alter $table)
		{
			$table->addColumn('state', 'varbinary', 64)->setDefault('');
		});
	}

	public function uninstallStep1(): void
	{
		$this->schemaManager()->dropTable('xf_wf_tuilink_link');
	}

	protected function createLinkTable(): void
	{
		$this->schemaManager()->createTable('xf_wf_tuilink_link', function (Create $table)
		{
			$table->addColumn('link_id', 'varbinary', 16);
			$table->addColumn('state', 'varbinary', 64);
			$table->addColumn('state_hash', 'varbinary', 64);
			$table->addColumn('challenge', 'varbinary', 64);
			$table->addColumn('code', 'varbinary', 255)->nullable()->setDefault(null);
			$table->addColumn('denied', 'tinyint', 3)->unsigned()->setDefault(0);
			$table->addColumn('expiry_date', 'int')->unsigned();
			$table->addPrimaryKey('link_id');
			$table->addUniqueKey('state_hash');
			$table->addKey('expiry_date');
		});
	}
}
